/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use calcard::{
    common::{PartialDateTime, timezone::Tz},
    icalendar::{
        ArchivedICalendar, ArchivedICalendarComponent, ArchivedICalendarEntry,
        ArchivedICalendarParameter, ArchivedICalendarProperty, ArchivedICalendarValue,
        ICalendarComponentType, ICalendarEntry, ICalendarParameterName, ICalendarProperty,
        ICalendarValue, dates::CalendarEvent,
    },
};
use common::{DavResource, Server, auth::AccessToken};
use dav_proto::{
    RequestHeaders,
    schema::{
        property::{CalDavProperty, CalendarData, DavProperty, TimeRange},
        request::{CalendarQuery, Filter, FilterOp, PropFind, Timezone},
    },
};
use groupware::{calendar::ArchivedCalendarEvent, hierarchy::DavHierarchy};
use http_proto::HttpResponse;
use hyper::StatusCode;
use jmap_proto::types::{acl::Acl, collection::Collection};
use std::{fmt::Write, slice::Iter, str::FromStr};
use store::{
    ahash::{AHashMap, AHashSet},
    write::serialize::rkyv_deserialize,
};
use trc::AddContext;

use crate::{
    DavError,
    common::{
        CalendarFilter, DavQuery,
        propfind::{PropFindItem, PropFindRequestHandler},
        uri::DavUriResource,
    },
};

use super::freebusy::freebusy_in_range;

pub(crate) trait CalendarQueryRequestHandler: Sync + Send {
    fn handle_calendar_query_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
        request: CalendarQuery,
    ) -> impl Future<Output = crate::Result<HttpResponse>> + Send;
}

impl CalendarQueryRequestHandler for Server {
    async fn handle_calendar_query_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
        request: CalendarQuery,
    ) -> crate::Result<HttpResponse> {
        // Validate URI
        let resource_ = self
            .validate_uri(access_token, headers.uri)
            .await?
            .into_owned_uri()?;
        let account_id = resource_.account_id;
        let resources = self
            .fetch_dav_resources(access_token, account_id, Collection::Calendar)
            .await
            .caused_by(trc::location!())?;
        let resource = resources
            .paths
            .by_name(
                resource_
                    .resource
                    .ok_or(DavError::Code(StatusCode::METHOD_NOT_ALLOWED))?,
            )
            .ok_or(DavError::Code(StatusCode::NOT_FOUND))?;
        if !resource.is_container() {
            return Err(DavError::Code(StatusCode::METHOD_NOT_ALLOWED));
        }

        // Obtain shared ids
        let shared_ids = if !access_token.is_member(account_id) {
            self.shared_containers(
                access_token,
                account_id,
                Collection::Calendar,
                [Acl::ReadItems],
                false,
            )
            .await
            .caused_by(trc::location!())?
            .into()
        } else {
            None
        };

        // Pre-filter by date range
        let filter_range = extract_filter_range(&request);

        // Obtain document ids in folder
        let mut items = Vec::with_capacity(16);
        for resource in resources.children(resource.document_id) {
            if shared_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(resource.document_id))
                && filter_range
                    .as_ref()
                    .is_none_or(|range| is_resource_in_time_range(resource, range))
            {
                items.push(PropFindItem::new(
                    resources.format_resource(resource),
                    account_id,
                    resource,
                ));
            }
        }

        // Extract the time range from the request
        let max_time_range = extract_data_range(&request.properties, filter_range);

        self.handle_dav_query(
            access_token,
            DavQuery::calendar_query(request, max_time_range, items, headers),
        )
        .await
    }
}

pub(crate) fn is_resource_in_time_range(resource: &DavResource, range: &TimeRange) -> bool {
    if let Some((start, end)) = resource.event_time_range() {
        // Check if either the start or end of the resource is within the range
        let range = range.start..=range.end;
        range.contains(&start) || range.contains(&end)
    } else {
        // If the resource does not have a time range, it is not in the range
        false
    }
}

fn extract_filter_range(query: &CalendarQuery) -> Option<TimeRange> {
    let mut range = TimeRange {
        start: i64::MAX,
        end: i64::MIN,
    };

    for filter in &query.filters {
        let op = match filter {
            Filter::Component { op, .. } => op,
            Filter::Property { op, .. } => op,
            Filter::Parameter { op, .. } => op,
            _ => continue,
        };
        if let FilterOp::TimeRange(date_range) = op {
            if date_range.start < range.start {
                range.start = date_range.start;
            }
            if date_range.end > range.end {
                range.end = date_range.end;
            }
        }
    }

    if range.start != i64::MAX {
        Some(range)
    } else {
        None
    }
}

fn extract_data_range(propfind: &PropFind, filter_range: Option<TimeRange>) -> Option<TimeRange> {
    let props = match propfind {
        PropFind::PropName => todo!(),
        PropFind::AllProp(props) | PropFind::Prop(props) => props,
    };

    for prop in props {
        if let DavProperty::CalDav(CalDavProperty::CalendarData(data)) = prop {
            let mut range = filter_range.unwrap_or(TimeRange {
                start: i64::MAX,
                end: i64::MIN,
            });

            for data_range in [&data.expand, &data.limit_recurrence, &data.limit_freebusy]
                .into_iter()
                .flatten()
            {
                if data_range.start < range.start {
                    range.start = data_range.start;
                }
                if data_range.end > range.end {
                    range.end = data_range.end;
                }
            }

            return if range.start != i64::MAX {
                Some(range)
            } else {
                None
            };
        }
    }

    filter_range
}

pub fn try_parse_tz(tz: &Timezone) -> Option<Tz> {
    match tz {
        Timezone::Name(value) | Timezone::Id(value) => Tz::from_str(value).ok(),
        Timezone::None => None,
    }
}

pub(crate) struct CalendarQueryHandler {
    default_tz: Tz,
    filtered_components: AHashSet<u16>,
    expanded_times: Vec<CalendarEvent<i64, i64>>,
}

impl CalendarQueryHandler {
    pub fn new(
        event: &ArchivedCalendarEvent,
        max_time_range: Option<TimeRange>,
        default_tz: Tz,
    ) -> Self {
        Self {
            default_tz,
            filtered_components: AHashSet::new(),
            expanded_times: max_time_range
                .map(|max_time_range| {
                    event
                        .data
                        .expand(default_tz, max_time_range)
                        .unwrap_or_else(|| {
                            let todo = "log error";
                            vec![]
                        })
                })
                .unwrap_or_default(),
        }
    }

    pub fn filter(&mut self, event: &ArchivedCalendarEvent, filters: &CalendarFilter) -> bool {
        let ical = &event.data.event;
        let mut is_all = true;
        let mut matches_one = false;

        for filter in filters {
            match filter {
                Filter::AnyOf => {
                    is_all = false;
                }
                Filter::AllOf => {
                    is_all = true;
                }
                Filter::Property { prop, op, comp } => {
                    let mut result = false;

                    for (_, comp) in find_components(ical, comp) {
                        if let Some(entry) = find_property(comp, prop) {
                            result = match op {
                                FilterOp::Exists => true,
                                FilterOp::Undefined => false,
                                FilterOp::TextMatch(text_match) => {
                                    let mut matched_any = false;

                                    for value in entry.values.iter() {
                                        if let Some(text) = value.as_text() {
                                            if text_match.matches(&text.to_lowercase()) {
                                                matched_any = true;
                                                break;
                                            }
                                        }
                                    }

                                    matched_any
                                }
                                FilterOp::TimeRange(range) => {
                                    if let Some(ArchivedICalendarValue::PartialDateTime(date)) =
                                        entry.values.first()
                                    {
                                        let tz = entry
                                            .tz_id()
                                            .and_then(|tz_id| Tz::from_str(tz_id).ok())
                                            .unwrap_or(self.default_tz);

                                        if let Some(date) = date
                                            .to_date_time()
                                            .and_then(|date| date.to_date_time_with_tz(tz))
                                        {
                                            let timestamp = date.timestamp();
                                            // RFC4791#9.9: start <= DTSTART AND end > DTSTART
                                            range.start <= timestamp && range.end > timestamp
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    }
                                }
                            };

                            if result {
                                break;
                            }
                        }
                    }

                    if result || matches!(op, FilterOp::Undefined) {
                        matches_one = true;
                    } else if is_all {
                        return false;
                    }
                }
                Filter::Parameter {
                    prop,
                    param,
                    op,
                    comp,
                } => {
                    let mut result = false;

                    for (_, comp) in find_components(ical, comp) {
                        if let Some(entry) =
                            find_property(comp, prop).and_then(|entry| find_parameter(entry, param))
                        {
                            result = match op {
                                FilterOp::Exists => true,
                                FilterOp::Undefined => false,
                                FilterOp::TextMatch(text_match) => {
                                    if let Some(text) = entry.as_text() {
                                        text_match.matches(&text.to_lowercase())
                                    } else {
                                        false
                                    }
                                }
                                FilterOp::TimeRange(_) => false,
                            };
                            if result {
                                break;
                            }
                        }
                    }

                    if result || matches!(op, FilterOp::Undefined) {
                        matches_one = true;
                    } else if is_all {
                        return false;
                    }
                }
                Filter::Component { comp, op } => {
                    let result = match op {
                        FilterOp::Exists => find_components(ical, comp).next().is_some(),
                        FilterOp::Undefined => find_components(ical, comp).next().is_none(),
                        FilterOp::TimeRange(range) => {
                            let matching_comp_ids = find_components(ical, comp)
                                .map(|(id, comp)| (id as u16, &comp.component_type))
                                .collect::<AHashMap<_, _>>();
                            if !matching_comp_ids.is_empty() {
                                let filtered_components = self
                                    .expanded_times
                                    .iter()
                                    .filter(|event| {
                                        matching_comp_ids.get(&event.comp_id).is_some_and(|ct| {
                                            range.is_in_range(
                                                ct == &&ICalendarComponentType::VTodo,
                                                event.start,
                                                event.end,
                                            )
                                        })
                                    })
                                    .map(|event| event.comp_id)
                                    .collect::<AHashSet<_>>();
                                if self.filtered_components.is_empty() {
                                    self.filtered_components = filtered_components;
                                } else {
                                    self.filtered_components
                                        .retain(|id| filtered_components.contains(id));
                                }
                                !self.filtered_components.is_empty()
                            } else {
                                false
                            }
                        }
                        FilterOp::TextMatch(_) => false,
                    };

                    if result {
                        matches_one = true;
                    } else if is_all {
                        return false;
                    }
                }
            }
        }

        is_all || matches_one
    }

    pub fn serialize_ical(&mut self, event: &ArchivedCalendarEvent, data: &CalendarData) -> String {
        let mut out = String::with_capacity(event.size.to_native() as usize);
        let _v = [0.into()];
        let mut component_iter: Iter<'_, rkyv::rend::u16_le> = _v.iter();
        let mut component_stack = Vec::with_capacity(4);

        if data.expand.is_some() {
            self.expanded_times
                .sort_unstable_by(|a, b| a.start.cmp(&b.start));
        }

        loop {
            if let Some(component_id) = component_iter.next() {
                let component_id = component_id.to_native();
                let component = event
                    .data
                    .event
                    .components
                    .get(component_id as usize)
                    .unwrap();

                // Skip filtered components
                if !self.filtered_components.is_empty()
                    && component.component_type.has_time_ranges()
                    && !self.filtered_components.contains(&component_id)
                {
                    continue;
                }

                // Limit recurrence override
                if let Some(limit_recurrence) = &data.limit_recurrence {
                    if component.is_recurrence_override()
                        && !self.expanded_times.iter().any(|event| {
                            event.comp_id == component_id
                                && limit_recurrence.is_in_range(
                                    component.component_type == ICalendarComponentType::VTodo,
                                    event.start,
                                    event.end,
                                )
                        })
                    {
                        continue;
                    }
                }

                // Limit freebusy
                if let Some(limit_recurrence) = &data.limit_freebusy {
                    if component.component_type == ICalendarComponentType::VFreebusy
                        && !self.expanded_times.iter().any(|event| {
                            event.comp_id == component_id
                                && limit_recurrence.is_in_range(false, event.start, event.end)
                        })
                    {
                        continue;
                    }
                }

                // Filter entries
                let mut entries = component
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        if data.properties.is_empty()
                            || component.component_type == ICalendarComponentType::VCalendar
                        {
                            Some((entry, true))
                        } else {
                            data.properties
                                .iter()
                                .find(|prop| {
                                    prop.component
                                        .as_ref()
                                        .is_none_or(|comp| comp == &component.component_type)
                                        && prop.name.as_ref().is_none_or(|name| name == &entry.name)
                                })
                                .map(|prop| (entry, !prop.no_value))
                        }
                    })
                    .peekable();

                // Expand recurrences
                let component_name = component.component_type.as_str();
                if let Some(expand) = &data.expand {
                    let is_recurrent = component.is_recurrent();
                    let is_recurrence_override = component.is_recurrence_override();
                    if is_recurrent || is_recurrence_override {
                        let is_todo = component.component_type == ICalendarComponentType::VTodo;
                        let mut has_duration = false;
                        let entries = entries
                            .filter(|(entry, _)| match &entry.name {
                                ArchivedICalendarProperty::Dtstart
                                | ArchivedICalendarProperty::Dtend
                                | ArchivedICalendarProperty::Exdate
                                | ArchivedICalendarProperty::Exrule
                                | ArchivedICalendarProperty::Rdate
                                | ArchivedICalendarProperty::Rrule
                                | ArchivedICalendarProperty::RecurrenceId => false,
                                ArchivedICalendarProperty::Due
                                | ArchivedICalendarProperty::Completed
                                | ArchivedICalendarProperty::Created => is_recurrent,
                                ArchivedICalendarProperty::Duration => {
                                    has_duration = true;
                                    true
                                }
                                _ => true,
                            })
                            .collect::<Vec<_>>();
                        for event in &self.expanded_times {
                            if event.comp_id == component_id
                                && expand.is_in_range(is_todo, event.start, event.end)
                            {
                                let _ = write!(&mut out, "BEGIN:{component_name}\r\n");

                                // Write DTSTART, DTEND and RECURRENCE-ID
                                let mut entry = ICalendarEntry {
                                    name: ICalendarProperty::Dtstart,
                                    params: vec![],
                                    values: vec![ICalendarValue::PartialDateTime(Box::new(
                                        PartialDateTime::from_utc_timestamp(event.start),
                                    ))],
                                };
                                let _ = entry.write_to(&mut out);
                                if is_recurrence_override {
                                    entry.name = ICalendarProperty::RecurrenceId;
                                    let _ = entry.write_to(&mut out);
                                }
                                if !has_duration {
                                    entry.name = ICalendarProperty::Dtend;
                                    entry.values = vec![ICalendarValue::PartialDateTime(Box::new(
                                        PartialDateTime::from_utc_timestamp(event.end),
                                    ))];
                                    let _ = entry.write_to(&mut out);
                                }

                                // Write other component entries
                                for (entry, with_value) in &entries {
                                    let _ = entry.write_to(&mut out, *with_value);
                                }
                                let _ = write!(&mut out, "END:{component_name}\r\n");
                            }
                        }
                        continue;
                    }
                }

                // Skip filtered components
                if entries.peek().is_none() {
                    continue;
                }

                let _ = write!(&mut out, "BEGIN:{component_name}\r\n");

                if data.limit_freebusy.is_none()
                    || component.component_type != ICalendarComponentType::VFreebusy
                {
                    for (entry, with_value) in entries {
                        let _ = entry.write_to(&mut out, with_value);
                    }
                } else {
                    // Filter freebusy
                    let range = data.limit_freebusy.unwrap();
                    for (entry, with_value) in entries {
                        if matches!(entry.name, ArchivedICalendarProperty::Freebusy) {
                            let mut fb_in_range =
                                freebusy_in_range(entry, &range, false, self.default_tz).peekable();
                            if fb_in_range.peek().is_none() {
                                continue;
                            } else {
                                let _ = ICalendarEntry {
                                    name: ICalendarProperty::Freebusy,
                                    params: rkyv_deserialize(&entry.params)
                                        .ok()
                                        .unwrap_or_default(),
                                    values: fb_in_range.collect(),
                                }
                                .write_to(&mut out);
                            }
                        } else {
                            let _ = entry.write_to(&mut out, with_value);
                        }
                    }
                }

                if !component.component_ids.is_empty() {
                    component_stack.push((component, component_iter));
                    component_iter = component.component_ids.iter();
                } else {
                    let _ = write!(&mut out, "END:{component_name}\r\n");
                }
            } else if let Some((component, iter)) = component_stack.pop() {
                let _ = write!(&mut out, "END:{}\r\n", component.component_type.as_str());
                component_iter = iter;
            } else {
                break;
            }
        }

        out
    }

    pub fn into_expanded_times(self) -> Vec<CalendarEvent<i64, i64>> {
        self.expanded_times
    }
}

#[inline(always)]
fn find_components<'x>(
    ical: &'x ArchivedICalendar,
    comp: &[ICalendarComponentType],
) -> impl Iterator<Item = (usize, &'x ArchivedICalendarComponent)> {
    // TODO: Properly expand the component type path
    let comp = comp
        .last()
        .copied()
        .unwrap_or(ICalendarComponentType::VCalendar);
    ical.components
        .iter()
        .enumerate()
        .filter(move |(_, entry)| {
            comp == ICalendarComponentType::VCalendar || entry.component_type == comp
        })
}

#[inline(always)]
fn find_property<'x>(
    comp: &'x ArchivedICalendarComponent,
    prop: &ICalendarProperty,
) -> Option<&'x ArchivedICalendarEntry> {
    comp.entries.iter().find(|entry| &entry.name == prop)
}

#[inline(always)]
fn find_parameter<'x>(
    entry: &'x ArchivedICalendarEntry,
    name: &ICalendarParameterName,
) -> Option<&'x ArchivedICalendarParameter> {
    entry.params.iter().find(|param| param.matches_name(name))
}
