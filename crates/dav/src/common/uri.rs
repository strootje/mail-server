/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::fmt::Display;

use common::{Server, auth::AccessToken};

use directory::backend::internal::manage::ManageDirectory;
use groupware::hierarchy::DavHierarchy;
use http_proto::request::decode_path_element;
use hyper::StatusCode;
use jmap_proto::types::collection::Collection;
use trc::AddContext;

use crate::{DavError, DavResourceName};

#[derive(Debug)]
pub(crate) struct UriResource<A, R> {
    pub collection: Collection,
    pub account_id: A,
    pub resource: R,
}

pub(crate) enum Urn {
    Lock(u64),
    Sync(u64),
}

pub(crate) type UnresolvedUri<'x> = UriResource<Option<u32>, Option<&'x str>>;
pub(crate) type OwnedUri<'x> = UriResource<u32, Option<&'x str>>;
pub(crate) type DocumentUri = UriResource<u32, u32>;

pub(crate) trait DavUriResource: Sync + Send {
    fn validate_uri<'x>(
        &self,
        access_token: &AccessToken,
        uri: &'x str,
    ) -> impl Future<Output = crate::Result<UnresolvedUri<'x>>> + Send;

    fn map_uri_resource(
        &self,
        access_token: &AccessToken,
        uri: OwnedUri<'_>,
    ) -> impl Future<Output = trc::Result<Option<DocumentUri>>> + Send;
}

impl DavUriResource for Server {
    async fn validate_uri<'x>(
        &self,
        access_token: &AccessToken,
        uri: &'x str,
    ) -> crate::Result<UnresolvedUri<'x>> {
        let (_, uri_parts) = uri
            .split_once("/dav/")
            .ok_or(DavError::Code(StatusCode::NOT_FOUND))?;

        let mut uri_parts = uri_parts
            .trim_end_matches('/')
            .splitn(3, '/')
            .filter(|x| !x.is_empty());
        let mut resource = UriResource {
            collection: uri_parts
                .next()
                .and_then(DavResourceName::parse)
                .ok_or(DavError::Code(StatusCode::NOT_FOUND))?
                .into(),
            account_id: None,
            resource: None,
        };
        if let Some(account) = uri_parts.next() {
            // Parse account id
            let account_id = if let Some(account_id) = account.strip_prefix('_') {
                account_id
                    .parse::<u32>()
                    .map_err(|_| DavError::Code(StatusCode::NOT_FOUND))?
            } else {
                let account = decode_path_element(account);
                if access_token.name == account {
                    access_token.primary_id
                } else {
                    self.store()
                        .get_principal_id(&account)
                        .await
                        .caused_by(trc::location!())?
                        .ok_or(DavError::Code(StatusCode::NOT_FOUND))?
                }
            };

            // Validate access
            if resource.collection != Collection::Principal
                && !access_token.has_access(account_id, resource.collection)
            {
                return Err(DavError::Code(StatusCode::FORBIDDEN));
            }

            // Obtain remaining path
            resource.account_id = Some(account_id);
            resource.resource = uri_parts.next();
        }

        Ok(resource)
    }

    async fn map_uri_resource(
        &self,
        access_token: &AccessToken,
        uri: OwnedUri<'_>,
    ) -> trc::Result<Option<DocumentUri>> {
        if let Some(resource) = uri.resource {
            if let Some(resource) = self
                .fetch_dav_resources(access_token, uri.account_id, uri.collection)
                .await
                .caused_by(trc::location!())?
                .paths
                .by_name(resource)
            {
                Ok(Some(DocumentUri {
                    collection: if resource.is_container() || uri.collection == Collection::FileNode
                    {
                        uri.collection
                    } else if uri.collection == Collection::Calendar {
                        Collection::CalendarEvent
                    } else {
                        Collection::ContactCard
                    },
                    account_id: uri.account_id,
                    resource: resource.document_id,
                }))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

impl<'x> UnresolvedUri<'x> {
    pub fn into_owned_uri(self) -> crate::Result<OwnedUri<'x>> {
        Ok(OwnedUri {
            collection: self.collection,
            account_id: self
                .account_id
                .ok_or(DavError::Code(StatusCode::FORBIDDEN))?,
            resource: self.resource,
        })
    }
}

impl OwnedUri<'_> {
    pub fn new_owned(
        collection: Collection,
        account_id: u32,
        resource: Option<&str>,
    ) -> OwnedUri<'_> {
        OwnedUri {
            collection,
            account_id,
            resource,
        }
    }
}

impl<A, R> UriResource<A, R> {
    pub fn collection_path(&self) -> &'static str {
        DavResourceName::from(self.collection).collection_path()
    }
}

impl Urn {
    pub fn parse(input: &str) -> Option<Self> {
        let inbox = input.strip_prefix("urn:stalwart:")?;
        let (kind, id) = inbox.split_once(':')?;
        match kind {
            "davlock" => u64::from_str_radix(id, 16).ok().map(Urn::Lock),
            "davsync" => u64::from_str_radix(id, 16).ok().map(Urn::Sync),
            _ => None,
        }
    }

    pub fn try_unwrap_lock(&self) -> Option<u64> {
        match self {
            Urn::Lock(id) => Some(*id),
            _ => None,
        }
    }

    pub fn try_unwrap_sync(&self) -> Option<u64> {
        match self {
            Urn::Sync(id) => Some(*id),
            _ => None,
        }
    }
}

impl Display for Urn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Urn::Lock(id) => write!(f, "urn:stalwart:davlock:{id:x}",),
            Urn::Sync(id) => write!(f, "urn:stalwart:davsync:{id:x}"),
        }
    }
}
