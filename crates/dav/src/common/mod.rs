/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use calcard::{
    icalendar::{ICalendarComponentType, ICalendarParameterName, ICalendarProperty},
    vcard::VCardParameterName,
};
use dav_proto::{
    Depth, RequestHeaders, Return,
    schema::{
        Namespace,
        property::{DavProperty, ReportSet, ResourceType, TimeRange},
        request::{
            AddressbookQuery, ArchivedDeadProperty, CalendarQuery, ExpandProperty, Filter,
            MultiGet, PropFind, SyncCollection, Timezone, VCardPropertyWithGroup,
        },
    },
};
use groupware::{
    calendar::{ArchivedCalendar, ArchivedCalendarEvent, Calendar, CalendarEvent},
    contact::{AddressBook, ArchivedAddressBook, ArchivedContactCard, ContactCard},
    file::{ArchivedFileNode, FileNode},
};
use jmap_proto::types::{collection::Collection, property::Property, value::ArchivedAclGrant};
use propfind::PropFindItem;
use rkyv::vec::ArchivedVec;
use store::{
    U32_LEN,
    write::{AlignedBytes, Archive, BatchBuilder, Operation, ValueClass, ValueOp},
};
use uri::{OwnedUri, Urn};

pub mod acl;
pub mod lock;
pub mod propfind;
pub mod uri;

#[derive(Default, Debug)]
pub(crate) struct DavQuery<'x> {
    pub resource: DavQueryResource<'x>,
    pub propfind: PropFind,
    pub from_change_id: Option<u64>,
    pub depth: usize,
    pub limit: Option<u32>,
    pub ret: Return,
    pub depth_no_root: bool,
    pub expand: bool,
}

#[derive(Default, Debug)]
pub(crate) enum DavQueryResource<'x> {
    Uri(OwnedUri<'x>),
    Multiget {
        parent_collection: Collection,
        hrefs: Vec<String>,
    },
    Query {
        filter: DavQueryFilter,
        parent_collection: Collection,
        items: Vec<PropFindItem>,
    },
    #[default]
    None,
}

pub(crate) type AddressbookFilter = Vec<Filter<(), VCardPropertyWithGroup, VCardParameterName>>;
pub(crate) type CalendarFilter =
    Vec<Filter<Vec<ICalendarComponentType>, ICalendarProperty, ICalendarParameterName>>;

#[derive(Debug)]
pub(crate) enum DavQueryFilter {
    Addressbook(AddressbookFilter),
    Calendar {
        filter: CalendarFilter,
        max_time_range: Option<TimeRange>,
        timezone: Timezone,
    },
}

pub(crate) trait ETag {
    fn etag(&self) -> String;
}

pub(crate) trait ExtractETag {
    fn etag(&self) -> Option<String>;
}

impl<T> ETag for Archive<T> {
    fn etag(&self) -> String {
        format!("\"{}\"", self.hash)
    }
}

impl ExtractETag for BatchBuilder {
    fn etag(&self) -> Option<String> {
        let p_value = u8::from(Property::Value);
        for op in self.ops().iter().rev() {
            match op {
                Operation::Value {
                    class: ValueClass::Property(p_id),
                    op: ValueOp::Set(value),
                } if *p_id == p_value => {
                    return value
                        .get(value.len() - U32_LEN..)
                        .map(|v| format!("\"{}\"", u32::from_be_bytes(v.try_into().unwrap())));
                }
                _ => {}
            }
        }

        None
    }
}

pub(crate) trait DavCollection {
    fn namespace(&self) -> Namespace;
}

impl DavCollection for Collection {
    fn namespace(&self) -> Namespace {
        match self {
            Collection::Calendar | Collection::CalendarEvent => Namespace::CalDav,
            Collection::AddressBook | Collection::ContactCard => Namespace::CardDav,
            _ => Namespace::Dav,
        }
    }
}

impl<'x> DavQuery<'x> {
    pub fn propfind(
        resource: OwnedUri<'x>,
        propfind: PropFind,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Uri(resource),
            propfind,
            depth: match headers.depth {
                Depth::Zero => 0,
                _ => 1,
            },
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            ..Default::default()
        }
    }

    pub fn multiget(
        multiget: MultiGet,
        collection: Collection,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Multiget {
                hrefs: multiget.hrefs,
                parent_collection: collection,
            },
            propfind: multiget.properties,
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            ..Default::default()
        }
    }

    pub fn addressbook_query(
        query: AddressbookQuery,
        items: Vec<PropFindItem>,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Query {
                filter: DavQueryFilter::Addressbook(query.filters),
                parent_collection: Collection::AddressBook,
                items,
            },
            propfind: query.properties,
            limit: query.limit,
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            ..Default::default()
        }
    }

    pub fn calendar_query(
        query: CalendarQuery,
        max_time_range: Option<TimeRange>,
        items: Vec<PropFindItem>,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Query {
                filter: DavQueryFilter::Calendar {
                    filter: query.filters,
                    timezone: query.timezone,
                    max_time_range,
                },
                parent_collection: Collection::Calendar,
                items,
            },
            propfind: query.properties,
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            ..Default::default()
        }
    }

    pub fn changes(
        resource: OwnedUri<'x>,
        changes: SyncCollection,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Uri(resource),
            propfind: changes.properties,
            from_change_id: changes
                .sync_token
                .as_deref()
                .and_then(Urn::parse)
                .and_then(|urn| urn.try_unwrap_sync())
                .unwrap_or_default()
                .into(),
            depth: if changes.level_inf { usize::MAX } else { 1 },
            limit: changes.limit,
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            expand: false,
        }
    }

    pub fn expand(
        resource: OwnedUri<'x>,
        expand: ExpandProperty,
        headers: RequestHeaders<'x>,
    ) -> Self {
        Self {
            resource: DavQueryResource::Uri(resource),
            propfind: PropFind::Prop(
                expand
                    .properties
                    .into_iter()
                    .filter_map(|item| {
                        if !matches!(item.property, DavProperty::DeadProperty(_)) {
                            Some(item.property)
                        } else {
                            None
                        }
                    })
                    .collect(),
            ),
            depth: match headers.depth {
                Depth::Zero => 0,
                _ => 1,
            },
            ret: headers.ret,
            depth_no_root: headers.depth_no_root,
            expand: true,
            ..Default::default()
        }
    }

    pub fn is_minimal(&self) -> bool {
        self.ret == Return::Minimal
    }
}

pub(crate) enum ArchivedResource<'x> {
    Calendar(Archive<&'x ArchivedCalendar>),
    CalendarEvent(Archive<&'x ArchivedCalendarEvent>),
    AddressBook(Archive<&'x ArchivedAddressBook>),
    ContactCard(Archive<&'x ArchivedContactCard>),
    FileNode(Archive<&'x ArchivedFileNode>),
}

impl<'x> ArchivedResource<'x> {
    pub fn from_archive(
        archive: &'x Archive<AlignedBytes>,
        collection: Collection,
    ) -> trc::Result<Self> {
        match collection {
            Collection::Calendar => archive
                .to_unarchived::<Calendar>()
                .map(ArchivedResource::Calendar),
            Collection::CalendarEvent => archive
                .to_unarchived::<CalendarEvent>()
                .map(ArchivedResource::CalendarEvent),
            Collection::AddressBook => archive
                .to_unarchived::<AddressBook>()
                .map(ArchivedResource::AddressBook),
            Collection::FileNode => archive
                .to_unarchived::<FileNode>()
                .map(ArchivedResource::FileNode),
            Collection::ContactCard => archive
                .to_unarchived::<ContactCard>()
                .map(ArchivedResource::ContactCard),
            _ => unreachable!(),
        }
    }

    pub fn acls(&self) -> Option<&ArchivedVec<ArchivedAclGrant>> {
        match self {
            Self::Calendar(archive) => Some(&archive.inner.acls),
            Self::AddressBook(archive) => Some(&archive.inner.acls),
            Self::FileNode(archive) => Some(&archive.inner.acls),
            _ => None,
        }
    }

    pub fn created(&self) -> i64 {
        match self {
            ArchivedResource::Calendar(archive) => archive.inner.created,
            ArchivedResource::CalendarEvent(archive) => archive.inner.created,
            ArchivedResource::AddressBook(archive) => archive.inner.created,
            ArchivedResource::ContactCard(archive) => archive.inner.created,
            ArchivedResource::FileNode(archive) => archive.inner.created,
        }
        .to_native()
    }

    pub fn modified(&self) -> i64 {
        match self {
            ArchivedResource::Calendar(archive) => archive.inner.modified,
            ArchivedResource::CalendarEvent(archive) => archive.inner.modified,
            ArchivedResource::AddressBook(archive) => archive.inner.modified,
            ArchivedResource::ContactCard(archive) => archive.inner.modified,
            ArchivedResource::FileNode(archive) => archive.inner.modified,
        }
        .to_native()
    }

    pub fn dead_properties(&self) -> &ArchivedDeadProperty {
        match self {
            ArchivedResource::Calendar(archive) => &archive.inner.dead_properties,
            ArchivedResource::CalendarEvent(archive) => &archive.inner.dead_properties,
            ArchivedResource::AddressBook(archive) => &archive.inner.dead_properties,
            ArchivedResource::ContactCard(archive) => &archive.inner.dead_properties,
            ArchivedResource::FileNode(archive) => &archive.inner.dead_properties,
        }
    }

    pub fn content_length(&self) -> Option<u32> {
        match self {
            ArchivedResource::FileNode(archive) => {
                archive.inner.file.as_ref().map(|f| f.size.to_native())
            }
            ArchivedResource::CalendarEvent(archive) => archive.inner.size.to_native().into(),
            ArchivedResource::ContactCard(archive) => archive.inner.size.to_native().into(),
            ArchivedResource::AddressBook(_) | ArchivedResource::Calendar(_) => None,
        }
    }

    pub fn content_type(&self) -> Option<&str> {
        match self {
            ArchivedResource::FileNode(archive) => archive
                .inner
                .file
                .as_ref()
                .and_then(|f| f.media_type.as_deref()),
            ArchivedResource::CalendarEvent(_) => "text/calendar".into(),
            ArchivedResource::ContactCard(_) => "text/vcard".into(),
            ArchivedResource::AddressBook(_) | ArchivedResource::Calendar(_) => None,
        }
    }

    pub fn display_name(&self, account_id: u32) -> Option<&str> {
        match self {
            ArchivedResource::Calendar(archive) => {
                Some(archive.inner.preferences(account_id).name.as_str())
            }
            ArchivedResource::CalendarEvent(archive) => archive.inner.display_name.as_deref(),
            ArchivedResource::AddressBook(archive) => archive.inner.display_name.as_deref(),
            ArchivedResource::ContactCard(archive) => archive.inner.display_name.as_deref(),
            ArchivedResource::FileNode(archive) => archive.inner.display_name.as_deref(),
        }
    }

    pub fn supported_report_set(&self) -> Option<Vec<ReportSet>> {
        match self {
            ArchivedResource::Calendar(_) => vec![
                ReportSet::SyncCollection,
                ReportSet::AclPrincipalPropSet,
                ReportSet::PrincipalMatch,
                ReportSet::ExpandProperty,
                ReportSet::CalendarQuery,
                ReportSet::CalendarMultiGet,
                ReportSet::FreeBusyQuery,
            ]
            .into(),
            ArchivedResource::AddressBook(_) => vec![
                ReportSet::SyncCollection,
                ReportSet::AclPrincipalPropSet,
                ReportSet::PrincipalMatch,
                ReportSet::ExpandProperty,
                ReportSet::AddressbookQuery,
                ReportSet::AddressbookMultiGet,
            ]
            .into(),
            ArchivedResource::FileNode(archive) if archive.inner.file.is_none() => vec![
                ReportSet::SyncCollection,
                ReportSet::AclPrincipalPropSet,
                ReportSet::PrincipalMatch,
            ]
            .into(),
            _ => None,
        }
    }

    pub fn resource_type(&self) -> Option<Vec<ResourceType>> {
        match self {
            ArchivedResource::Calendar(_) => {
                vec![ResourceType::Collection, ResourceType::Calendar].into()
            }
            ArchivedResource::AddressBook(_) => {
                vec![ResourceType::Collection, ResourceType::AddressBook].into()
            }
            ArchivedResource::FileNode(archive) if archive.inner.file.is_none() => {
                vec![ResourceType::Collection].into()
            }
            _ => None,
        }
    }
}
