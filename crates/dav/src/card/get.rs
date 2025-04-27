/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::{Server, auth::AccessToken};
use dav_proto::{RequestHeaders, schema::property::Rfc1123DateTime};
use groupware::{contact::ContactCard, hierarchy::DavHierarchy};
use http_proto::HttpResponse;
use hyper::StatusCode;
use jmap_proto::types::{acl::Acl, collection::Collection};
use trc::AddContext;

use crate::{
    DavError, DavMethod,
    common::{
        ETag,
        lock::{LockRequestHandler, ResourceState},
        uri::DavUriResource,
    },
};

pub(crate) trait CardGetRequestHandler: Sync + Send {
    fn handle_card_get_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
        is_head: bool,
    ) -> impl Future<Output = crate::Result<HttpResponse>> + Send;
}

impl CardGetRequestHandler for Server {
    async fn handle_card_get_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
        is_head: bool,
    ) -> crate::Result<HttpResponse> {
        // Validate URI
        let resource_ = self
            .validate_uri(access_token, headers.uri)
            .await?
            .into_owned_uri()?;
        let account_id = resource_.account_id;
        let resources = self
            .fetch_dav_resources(access_token, account_id, Collection::AddressBook)
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
        if resource.is_container() {
            return Err(DavError::Code(StatusCode::METHOD_NOT_ALLOWED));
        }

        // Validate ACL
        if !access_token.is_member(account_id)
            && !self
                .has_access_to_document(
                    access_token,
                    account_id,
                    Collection::AddressBook,
                    resource.parent_id.unwrap(),
                    Acl::ReadItems,
                )
                .await
                .caused_by(trc::location!())?
        {
            return Err(DavError::Code(StatusCode::FORBIDDEN));
        }

        // Fetch card
        let card_ = self
            .get_archive(account_id, Collection::ContactCard, resource.document_id)
            .await
            .caused_by(trc::location!())?
            .ok_or(DavError::Code(StatusCode::NOT_FOUND))?;
        let card = card_
            .unarchive::<ContactCard>()
            .caused_by(trc::location!())?;

        // Validate headers
        let etag = card_.etag();
        self.validate_headers(
            access_token,
            &headers,
            vec![ResourceState {
                account_id,
                collection: Collection::ContactCard,
                document_id: resource.document_id.into(),
                etag: etag.clone().into(),
                path: resource_.resource.unwrap(),
                ..Default::default()
            }],
            Default::default(),
            DavMethod::GET,
        )
        .await?;

        let response = HttpResponse::new(StatusCode::OK)
            .with_content_type("text/vcard; charset=utf-8")
            .with_etag(etag)
            .with_last_modified(Rfc1123DateTime::new(i64::from(card.modified)).to_string());

        let vcard = card.card.to_string();

        if !is_head {
            Ok(response.with_binary_body(vcard))
        } else {
            Ok(response.with_content_length(vcard.len()))
        }
    }
}
