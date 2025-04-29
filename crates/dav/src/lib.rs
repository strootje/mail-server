/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

pub mod calendar;
pub mod card;
pub mod common;
pub mod file;
pub mod principal;
pub mod request;

use dav_proto::schema::{
    request::DavPropertyValue,
    response::{Condition, List, Prop, PropStat, ResponseDescription, Status},
};
use groupware::DavResourceName;
use hyper::{Method, StatusCode};
use store::ahash::AHashMap;

pub(crate) type Result<T> = std::result::Result<T, DavError>;

#[derive(Debug, Clone, Copy)]
pub enum DavMethod {
    GET,
    PUT,
    POST,
    DELETE,
    HEAD,
    PATCH,
    PROPFIND,
    PROPPATCH,
    REPORT,
    MKCOL,
    MKCALENDAR,
    COPY,
    MOVE,
    LOCK,
    UNLOCK,
    OPTIONS,
    ACL,
}

pub(crate) enum DavError {
    Parse(dav_proto::parser::Error),
    Internal(trc::Error),
    Condition(DavErrorCondition),
    Code(StatusCode),
}

struct DavErrorCondition {
    pub code: StatusCode,
    pub condition: Condition,
}

impl From<DavErrorCondition> for DavError {
    fn from(value: DavErrorCondition) -> Self {
        DavError::Condition(value)
    }
}

impl From<Condition> for DavErrorCondition {
    fn from(value: Condition) -> Self {
        DavErrorCondition {
            code: StatusCode::CONFLICT,
            condition: value,
        }
    }
}

impl DavErrorCondition {
    pub fn new(code: StatusCode, condition: impl Into<Condition>) -> Self {
        DavErrorCondition {
            code,
            condition: condition.into(),
        }
    }
}

impl DavMethod {
    pub fn parse(method: &Method) -> Option<Self> {
        match *method {
            Method::GET => Some(DavMethod::GET),
            Method::PUT => Some(DavMethod::PUT),
            Method::DELETE => Some(DavMethod::DELETE),
            Method::OPTIONS => Some(DavMethod::OPTIONS),
            Method::POST => Some(DavMethod::POST),
            Method::PATCH => Some(DavMethod::PATCH),
            Method::HEAD => Some(DavMethod::HEAD),
            _ => {
                hashify::tiny_map!(method.as_str().as_bytes(),
                    "PROPFIND" => DavMethod::PROPFIND,
                    "PROPPATCH" => DavMethod::PROPPATCH,
                    "REPORT" => DavMethod::REPORT,
                    "MKCOL" => DavMethod::MKCOL,
                    "MKCALENDAR" => DavMethod::MKCALENDAR,
                    "COPY" => DavMethod::COPY,
                    "MOVE" => DavMethod::MOVE,
                    "LOCK" => DavMethod::LOCK,
                    "UNLOCK" => DavMethod::UNLOCK,
                    "ACL" => DavMethod::ACL
                )
            }
        }
    }

    #[inline]
    pub fn has_body(self) -> bool {
        matches!(
            self,
            DavMethod::PUT
                | DavMethod::POST
                | DavMethod::PATCH
                | DavMethod::PROPPATCH
                | DavMethod::PROPFIND
                | DavMethod::REPORT
                | DavMethod::LOCK
                | DavMethod::ACL
                | DavMethod::MKCALENDAR
        )
    }
}

#[derive(Debug, Default)]
pub struct PropStatBuilder {
    propstats: AHashMap<(StatusCode, Option<Condition>, Option<String>), Vec<DavPropertyValue>>,
}

impl PropStatBuilder {
    pub fn insert_ok(&mut self, prop: impl Into<DavPropertyValue>) -> &mut Self {
        self.propstats
            .entry((StatusCode::OK, None, None))
            .or_default()
            .push(prop.into());
        self
    }

    pub fn insert_with_status(
        &mut self,
        prop: impl Into<DavPropertyValue>,
        status: StatusCode,
    ) -> &mut Self {
        self.propstats
            .entry((status, None, None))
            .or_default()
            .push(prop.into());
        self
    }

    pub fn insert_error_with_description(
        &mut self,
        prop: impl Into<DavPropertyValue>,
        status: StatusCode,
        description: impl Into<String>,
    ) -> &mut Self {
        self.propstats
            .entry((status, None, Some(description.into())))
            .or_default()
            .push(prop.into());
        self
    }

    pub fn insert_precondition_failed(
        &mut self,
        prop: impl Into<DavPropertyValue>,
        status: StatusCode,
        condition: impl Into<Condition>,
    ) -> &mut Self {
        self.propstats
            .entry((status, Some(condition.into()), None))
            .or_default()
            .push(prop.into());
        self
    }

    pub fn insert_precondition_failed_with_description(
        &mut self,
        prop: impl Into<DavPropertyValue>,
        status: StatusCode,
        condition: impl Into<Condition>,
        description: impl Into<String>,
    ) -> &mut Self {
        self.propstats
            .entry((status, Some(condition.into()), Some(description.into())))
            .or_default()
            .push(prop.into());
        self
    }

    pub fn build(self) -> Vec<PropStat> {
        self.propstats
            .into_iter()
            .map(|((status, condition, description), props)| PropStat {
                prop: Prop(List(props)),
                status: Status(status),
                error: condition,
                response_description: description.map(ResponseDescription),
            })
            .collect()
    }
}
