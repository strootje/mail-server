/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::{Server, auth::AccessToken, sharing::EffectiveAcl};
use dav_proto::RequestHeaders;
use groupware::{
    DestroyArchive,
    calendar::{Calendar, CalendarEvent},
    hierarchy::DavHierarchy,
};
use http_proto::HttpResponse;
use hyper::StatusCode;
use jmap_proto::types::{acl::Acl, collection::Collection};
use store::write::BatchBuilder;
use trc::AddContext;

use crate::{
    DavError, DavMethod,
    common::{
        ETag,
        lock::{LockRequestHandler, ResourceState},
        uri::DavUriResource,
    },
};

pub(crate) trait CalendarDeleteRequestHandler: Sync + Send {
    fn handle_calendar_delete_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
    ) -> impl Future<Output = crate::Result<HttpResponse>> + Send;
}

impl CalendarDeleteRequestHandler for Server {
    async fn handle_calendar_delete_request(
        &self,
        access_token: &AccessToken,
        headers: RequestHeaders<'_>,
    ) -> crate::Result<HttpResponse> {
        // Validate URI
        let resource = self
            .validate_uri(access_token, headers.uri)
            .await?
            .into_owned_uri()?;
        let account_id = resource.account_id;
        let delete_path = resource
            .resource
            .filter(|r| !r.is_empty())
            .ok_or(DavError::Code(StatusCode::FORBIDDEN))?;
        let resources = self
            .fetch_dav_resources(access_token, account_id, Collection::Calendar)
            .await
            .caused_by(trc::location!())?;

        // Check resource type
        let delete_resource = resources
            .paths
            .by_name(delete_path)
            .ok_or(DavError::Code(StatusCode::FORBIDDEN))?;
        let document_id = delete_resource.document_id;

        // Fetch entry
        let mut batch = BatchBuilder::new();
        if delete_resource.is_container() {
            let calendar_ = self
                .get_archive(account_id, Collection::Calendar, document_id)
                .await
                .caused_by(trc::location!())?
                .ok_or(DavError::Code(StatusCode::NOT_FOUND))?;

            let calendar = calendar_
                .to_unarchived::<Calendar>()
                .caused_by(trc::location!())?;

            // Validate ACL
            if !access_token.is_member(account_id)
                && !calendar
                    .inner
                    .acls
                    .effective_acl(access_token)
                    .contains_all([Acl::Delete, Acl::RemoveItems].into_iter())
            {
                return Err(DavError::Code(StatusCode::FORBIDDEN));
            }

            // Validate headers
            self.validate_headers(
                access_token,
                &headers,
                vec![ResourceState {
                    account_id,
                    collection: Collection::Calendar,
                    document_id: document_id.into(),
                    etag: calendar.etag().into(),
                    path: delete_path,
                    ..Default::default()
                }],
                Default::default(),
                DavMethod::DELETE,
            )
            .await?;

            // Delete addresscalendar and events
            DestroyArchive(calendar)
                .delete_with_events(
                    self,
                    access_token,
                    account_id,
                    document_id,
                    resources
                        .subtree(delete_path)
                        .filter(|r| !r.is_container())
                        .map(|r| r.document_id)
                        .collect::<Vec<_>>(),
                    &mut batch,
                )
                .await
                .caused_by(trc::location!())?;
        } else {
            // Validate ACL
            let addresscalendar_id = delete_resource.parent_id.unwrap();
            if !access_token.is_member(account_id)
                && !self
                    .has_access_to_document(
                        access_token,
                        account_id,
                        Collection::Calendar,
                        addresscalendar_id,
                        Acl::RemoveItems,
                    )
                    .await
                    .caused_by(trc::location!())?
            {
                return Err(DavError::Code(StatusCode::FORBIDDEN));
            }

            let event_ = self
                .get_archive(account_id, Collection::CalendarEvent, document_id)
                .await
                .caused_by(trc::location!())?
                .ok_or(DavError::Code(StatusCode::NOT_FOUND))?;

            // Validate headers
            self.validate_headers(
                access_token,
                &headers,
                vec![ResourceState {
                    account_id,
                    collection: Collection::CalendarEvent,
                    document_id: document_id.into(),
                    etag: event_.etag().into(),
                    path: delete_path,
                    ..Default::default()
                }],
                Default::default(),
                DavMethod::DELETE,
            )
            .await?;

            // Delete event
            DestroyArchive(
                event_
                    .to_unarchived::<CalendarEvent>()
                    .caused_by(trc::location!())?,
            )
            .delete(
                access_token,
                account_id,
                document_id,
                addresscalendar_id,
                &mut batch,
            )
            .caused_by(trc::location!())?;
        }

        self.commit_batch(batch).await.caused_by(trc::location!())?;

        Ok(HttpResponse::new(StatusCode::NO_CONTENT))
    }
}
