/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::outbound::client::{SmtpClient, from_error_status, from_mail_send_error};
use crate::outbound::dane::dnssec::TlsaLookup;
use crate::outbound::lookup::DnsLookup;
use crate::outbound::mta_sts::lookup::MtaStsLookup;
use crate::outbound::mta_sts::verify::VerifyPolicy;
use crate::outbound::{client::StartTlsResult, dane::verify::TlsaVerify};
use crate::queue::dsn::SendDsn;
use crate::queue::spool::{LOCK_EXPIRY, SmtpSpool};
use crate::queue::throttle::IsAllowed;
use crate::reporting::SmtpReporting;
use common::Server;
use common::config::{
    server::ServerProtocol,
    smtp::{queue::RequireOptional, report::AggregateFrequency},
};
use common::ipc::{PolicyType, QueueEvent, QueueEventStatus, TlsEvent};

use compact_str::ToCompactString;
use mail_auth::{
    mta_sts::TlsRpt,
    report::tlsrpt::{FailureDetails, ResultType},
};
use rand::Rng;
use smtp_proto::MAIL_REQUIRETLS;
use std::sync::Arc;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use store::write::{BatchBuilder, QueueClass, ValueClass, now};
use trc::{DaneEvent, DeliveryEvent, MtaStsEvent, ServerEvent, TlsRptEvent};

use crate::{
    queue::{ErrorDetails, Message},
    reporting::tls::TlsRptOptions,
};

use super::{NextHop, TlsStrategy, lookup::ToNextHop, mta_sts, session::SessionParams};
use crate::queue::{Domain, Error, FROM_REPORT, QueueEnvelope, QueuedMessage, Status};

impl QueuedMessage {
    pub fn try_deliver(self, server: Server) {
        #![allow(clippy::large_futures)]
        tokio::spawn(async move {
            // Lock queue event
            let queue_id = self.queue_id;
            let status = if server.try_lock_event(queue_id).await {
                if let Some(mut message) = server.read_message(queue_id).await {
                    // Generate span id
                    message.span_id = server.inner.data.span_id_gen.generate();
                    let span_id = message.span_id;

                    trc::event!(
                        Delivery(DeliveryEvent::AttemptStart),
                        SpanId = message.span_id,
                        QueueId = message.queue_id,
                        From = if !message.return_path.is_empty() {
                            trc::Value::String(message.return_path.as_str().into())
                        } else {
                            trc::Value::String("<>".into())
                        },
                        To = message
                            .recipients
                            .iter()
                            .filter_map(|r| {
                                if matches!(
                                    r.status,
                                    Status::Scheduled | Status::TemporaryFailure(_)
                                ) {
                                    Some(trc::Value::String(r.address_lcase.as_str().into()))
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>(),
                        Size = message.size,
                        Total = message.recipients.len(),
                    );

                    // Attempt delivery
                    let start_time = Instant::now();
                    let queue_event = self.deliver_task(server.clone(), message).await;

                    trc::event!(
                        Delivery(DeliveryEvent::AttemptEnd),
                        SpanId = span_id,
                        Elapsed = start_time.elapsed(),
                    );

                    // Unlock event
                    server.unlock_event(queue_id).await;

                    queue_event
                } else {
                    // Message no longer exists, delete queue event.
                    let mut batch = BatchBuilder::new();
                    batch.clear(ValueClass::Queue(QueueClass::MessageEvent(
                        store::write::QueueEvent {
                            due: self.due,
                            queue_id: self.queue_id,
                        },
                    )));

                    if let Err(err) = server.store().write(batch.build_all()).await {
                        trc::error!(
                            err.details("Failed to delete queue event.")
                                .caused_by(trc::location!())
                        );
                    }

                    // Unlock event
                    server.unlock_event(queue_id).await;

                    QueueEventStatus::Completed
                }
            } else {
                QueueEventStatus::Locked {
                    until: now() + LOCK_EXPIRY + rand::rng().random_range(5..10),
                }
            };

            // Notify queue manager
            if server
                .inner
                .ipc
                .queue_tx
                .send(QueueEvent::WorkerDone { queue_id, status })
                .await
                .is_err()
            {
                trc::event!(
                    Server(ServerEvent::ThreadError),
                    Reason = "Channel closed.",
                    CausedBy = trc::location!(),
                );
            }
        });
    }

    async fn deliver_task(self, server: Server, mut message: Message) -> QueueEventStatus {
        // Check that the message still has recipients to be delivered
        let has_pending_delivery = message.has_pending_delivery();
        let span_id = message.span_id;

        // Send any due Delivery Status Notifications
        server.send_dsn(&mut message).await;

        if has_pending_delivery {
            // Re-queue the message if its not yet due for delivery
            let due = message.next_delivery_event();
            if due > now() {
                // Save changes
                message
                    .save_changes(&server, self.due.into(), due.into())
                    .await;
                return QueueEventStatus::Deferred;
            }
        } else {
            trc::event!(
                Delivery(DeliveryEvent::Completed),
                SpanId = span_id,
                Elapsed = trc::Value::Duration((now() - message.created) * 1000)
            );

            // All message recipients expired, do not re-queue. (DSN has been already sent)
            message.remove(&server, self.due).await;

            return QueueEventStatus::Completed;
        }

        // Throttle sender
        for throttle in &server.core.smtp.queue.outbound_limiters.sender {
            if let Err(retry_at) = server.is_allowed(throttle, &message, message.span_id).await {
                // Save changes to disk
                let next_event = std::cmp::min(
                    retry_at,
                    message.next_event_after(now()).unwrap_or(u64::MAX),
                );

                trc::event!(
                    Delivery(DeliveryEvent::RateLimitExceeded),
                    Id = throttle.id.clone(),
                    SpanId = span_id,
                    NextRetry = trc::Value::Timestamp(next_event)
                );

                message
                    .save_changes(&server, self.due.into(), next_event.into())
                    .await;

                return QueueEventStatus::Deferred;
            }
        }

        let queue_config = &server.core.smtp.queue;
        let no_ip = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
        let mut recipients = std::mem::take(&mut message.recipients);
        'next_domain: for domain_idx in 0..message.domains.len() {
            // Only process domains due for delivery
            let domain = &message.domains[domain_idx];
            if !matches!(&domain.status, Status::Scheduled | Status::TemporaryFailure(_)
                if domain.retry.due <= now())
            {
                continue;
            }

            trc::event!(
                Delivery(DeliveryEvent::DomainDeliveryStart),
                SpanId = message.span_id,
                Domain = domain.domain.clone(),
                Total = domain.retry.inner,
            );

            // Build envelope
            let mut envelope = QueueEnvelope::new(&message, domain_idx);

            // Throttle recipient domain
            for throttle in &queue_config.outbound_limiters.rcpt {
                if let Err(retry_at) = server
                    .is_allowed(throttle, &envelope, message.span_id)
                    .await
                {
                    trc::event!(
                        Delivery(DeliveryEvent::RateLimitExceeded),
                        Id = throttle.id.clone(),
                        SpanId = span_id,
                        Domain = domain.domain.clone(),
                    );

                    message.domains[domain_idx].set_rate_limiter_error(retry_at);
                    continue 'next_domain;
                }
            }

            // Obtain next hop
            let (mut remote_hosts, is_smtp) = match server
                .eval_if::<String, _>(&queue_config.next_hop, &envelope, message.span_id)
                .await
                .and_then(|name| server.get_relay_host(&name, message.span_id))
            {
                Some(next_hop) if next_hop.protocol == ServerProtocol::Http => {
                    // Deliver message locally
                    let delivery_result = message
                        .deliver_local(
                            recipients
                                .iter_mut()
                                .filter(|r| r.domain_idx == domain_idx as u32),
                            &server,
                        )
                        .await;

                    // Update status for the current domain and continue with the next one
                    let schedule = server
                        .eval_if::<Vec<Duration>, _>(
                            &queue_config.retry,
                            &envelope,
                            message.span_id,
                        )
                        .await
                        .unwrap_or_else(|| vec![Duration::from_secs(60)]);
                    message.domains[domain_idx].set_status(delivery_result, &schedule);
                    continue 'next_domain;
                }
                Some(next_hop) => (
                    vec![NextHop::Relay(next_hop)],
                    next_hop.protocol == ServerProtocol::Smtp,
                ),
                None => (Vec::with_capacity(0), true),
            };

            // Prepare TLS strategy
            let mut tls_strategy = TlsStrategy {
                mta_sts: server
                    .eval_if(&queue_config.tls.mta_sts, &envelope, message.span_id)
                    .await
                    .unwrap_or(RequireOptional::Optional),
                ..Default::default()
            };
            let allow_invalid_certs = server
                .eval_if(&queue_config.tls.invalid_certs, &envelope, message.span_id)
                .await
                .unwrap_or(false);

            // Obtain TLS reporting
            let tls_report = match server
                .eval_if(
                    &server.core.smtp.report.tls.send,
                    &envelope,
                    message.span_id,
                )
                .await
                .unwrap_or(AggregateFrequency::Never)
            {
                interval @ (AggregateFrequency::Hourly
                | AggregateFrequency::Daily
                | AggregateFrequency::Weekly)
                    if is_smtp && (message.flags & FROM_REPORT == 0) =>
                {
                    let time = Instant::now();
                    match server
                        .core
                        .smtp
                        .resolvers
                        .dns
                        .txt_lookup::<TlsRpt>(
                            format!("_smtp._tls.{}.", domain.domain),
                            Some(&server.inner.cache.dns_txt),
                        )
                        .await
                    {
                        Ok(record) => {
                            trc::event!(
                                TlsRpt(TlsRptEvent::RecordFetch),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Details = record
                                    .rua
                                    .iter()
                                    .map(|uri| trc::Value::from(match uri {
                                        mail_auth::mta_sts::ReportUri::Mail(uri)
                                        | mail_auth::mta_sts::ReportUri::Http(uri) =>
                                            uri.to_string(),
                                    }))
                                    .collect::<Vec<_>>(),
                                Elapsed = time.elapsed(),
                            );

                            TlsRptOptions { record, interval }.into()
                        }
                        Err(mail_auth::Error::DnsRecordNotFound(_)) => {
                            trc::event!(
                                TlsRpt(TlsRptEvent::RecordNotFound),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Elapsed = time.elapsed(),
                            );
                            None
                        }
                        Err(err) => {
                            trc::event!(
                                TlsRpt(TlsRptEvent::RecordFetchError),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                CausedBy = trc::Error::from(err),
                                Elapsed = time.elapsed(),
                            );
                            None
                        }
                    }
                }
                _ => None,
            };

            // Obtain MTA-STS policy for domain
            let mta_sts_policy = if tls_strategy.try_mta_sts() && is_smtp {
                let time = Instant::now();
                match server
                    .lookup_mta_sts_policy(
                        &domain.domain,
                        server
                            .eval_if(&queue_config.timeout.mta_sts, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(10 * 60)),
                    )
                    .await
                {
                    Ok(mta_sts_policy) => {
                        trc::event!(
                            MtaSts(MtaStsEvent::PolicyFetch),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Strict = mta_sts_policy.enforce(),
                            Details = mta_sts_policy
                                .mx
                                .iter()
                                .map(|mx| trc::Value::String(mx.to_compact_string()))
                                .collect::<Vec<_>>(),
                            Elapsed = time.elapsed(),
                        );

                        mta_sts_policy.into()
                    }
                    Err(err) => {
                        // Report MTA-STS error
                        let strict = tls_strategy.is_mta_sts_required();
                        if let Some(tls_report) = &tls_report {
                            match &err {
                                mta_sts::Error::Dns(mail_auth::Error::DnsRecordNotFound(_)) => {
                                    if strict {
                                        server.schedule_report(TlsEvent {
                                            policy: PolicyType::Sts(None),
                                            domain: domain.domain.to_string(),
                                            failure: FailureDetails::new(ResultType::Other)
                                                .with_failure_reason_code(
                                                    "MTA-STS is required and no policy was found.",
                                                )
                                                .into(),
                                            tls_record: tls_report.record.clone(),
                                            interval: tls_report.interval,
                                        })
                                        .await;
                                    }
                                }
                                mta_sts::Error::Dns(mail_auth::Error::DnsError(_)) => (),
                                _ => {
                                    server
                                        .schedule_report(TlsEvent {
                                            policy: PolicyType::Sts(None),
                                            domain: domain.domain.to_string(),
                                            failure: FailureDetails::new(&err)
                                                .with_failure_reason_code(err.to_string())
                                                .into(),
                                            tls_record: tls_report.record.clone(),
                                            interval: tls_report.interval,
                                        })
                                        .await;
                                }
                            }
                        }

                        match &err {
                            mta_sts::Error::Dns(mail_auth::Error::DnsRecordNotFound(_)) => {
                                trc::event!(
                                    MtaSts(MtaStsEvent::PolicyNotFound),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            }
                            mta_sts::Error::Dns(err) => {
                                trc::event!(
                                    MtaSts(MtaStsEvent::PolicyFetchError),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    CausedBy = trc::Error::from(err.clone()),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            }
                            mta_sts::Error::Http(err) => {
                                trc::event!(
                                    MtaSts(MtaStsEvent::PolicyFetchError),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Reason = err.to_string(),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            }
                            mta_sts::Error::InvalidPolicy(reason) => {
                                trc::event!(
                                    MtaSts(MtaStsEvent::InvalidPolicy),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Reason = reason.clone(),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            }
                        }

                        if strict {
                            let schedule = server
                                .eval_if::<Vec<Duration>, _>(
                                    &queue_config.retry,
                                    &envelope,
                                    message.span_id,
                                )
                                .await
                                .unwrap_or_else(|| vec![Duration::from_secs(60)]);
                            message.domains[domain_idx].set_status(err, &schedule);
                            continue 'next_domain;
                        }

                        None
                    }
                }
            } else {
                None
            };

            // Obtain remote hosts list
            let mx_list;
            if is_smtp && remote_hosts.is_empty() {
                // Lookup MX
                let time = Instant::now();
                mx_list = match server
                    .core
                    .smtp
                    .resolvers
                    .dns
                    .mx_lookup(domain.domain.as_str(), Some(&server.inner.cache.dns_mx))
                    .await
                {
                    Ok(mx) => mx,
                    Err(mail_auth::Error::DnsRecordNotFound(_)) => {
                        trc::event!(
                            Delivery(DeliveryEvent::MxLookupFailed),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Details = "No MX records were found, attempting implicit MX.",
                            Elapsed = time.elapsed(),
                        );

                        Arc::new(vec![])
                    }
                    Err(err) => {
                        trc::event!(
                            Delivery(DeliveryEvent::MxLookupFailed),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            CausedBy = trc::Error::from(err.clone()),
                            Elapsed = time.elapsed(),
                        );

                        let schedule = server
                            .eval_if::<Vec<Duration>, _>(
                                &queue_config.retry,
                                &envelope,
                                message.span_id,
                            )
                            .await
                            .unwrap_or_else(|| vec![Duration::from_secs(60)]);
                        message.domains[domain_idx].set_status(err, &schedule);
                        continue 'next_domain;
                    }
                };

                if let Some(remote_hosts_) = mx_list.to_remote_hosts(
                    &domain.domain,
                    server
                        .eval_if(&queue_config.max_mx, &envelope, message.span_id)
                        .await
                        .unwrap_or(5),
                ) {
                    trc::event!(
                        Delivery(DeliveryEvent::MxLookup),
                        SpanId = message.span_id,
                        Domain = domain.domain.clone(),
                        Details = remote_hosts_
                            .iter()
                            .map(|h| trc::Value::String(h.hostname().into()))
                            .collect::<Vec<_>>(),
                        Elapsed = time.elapsed(),
                    );
                    remote_hosts = remote_hosts_;
                } else {
                    trc::event!(
                        Delivery(DeliveryEvent::NullMx),
                        SpanId = message.span_id,
                        Domain = domain.domain.clone(),
                        Elapsed = time.elapsed(),
                    );

                    let schedule = server
                        .eval_if::<Vec<Duration>, _>(
                            &queue_config.retry,
                            &envelope,
                            message.span_id,
                        )
                        .await
                        .unwrap_or_else(|| vec![Duration::from_secs(60)]);
                    message.domains[domain_idx].set_status(
                        Status::PermanentFailure(Error::DnsError(
                            "Domain does not accept messages (null MX)".into(),
                        )),
                        &schedule,
                    );
                    continue 'next_domain;
                }
            }

            // Try delivering message
            let max_multihomed = server
                .eval_if(&queue_config.max_multihomed, &envelope, message.span_id)
                .await
                .unwrap_or(2);
            let mut last_status = Status::Scheduled;
            'next_host: for remote_host in &remote_hosts {
                // Validate MTA-STS
                envelope.mx = remote_host.hostname();
                if let Some(mta_sts_policy) = &mta_sts_policy {
                    let strict = mta_sts_policy.enforce();
                    if !mta_sts_policy.verify(envelope.mx) {
                        // Report MTA-STS failed verification
                        if let Some(tls_report) = &tls_report {
                            server
                                .schedule_report(TlsEvent {
                                    policy: mta_sts_policy.into(),
                                    domain: domain.domain.to_string(),
                                    failure: FailureDetails::new(ResultType::ValidationFailure)
                                        .with_receiving_mx_hostname(envelope.mx)
                                        .with_failure_reason_code("MX not authorized by policy.")
                                        .into(),
                                    tls_record: tls_report.record.clone(),
                                    interval: tls_report.interval,
                                })
                                .await;
                        }

                        trc::event!(
                            MtaSts(MtaStsEvent::NotAuthorized),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Hostname = envelope.mx.to_string(),
                            Details = mta_sts_policy
                                .mx
                                .iter()
                                .map(|mx| trc::Value::String(mx.to_compact_string()))
                                .collect::<Vec<_>>(),
                            Strict = strict,
                        );

                        if strict {
                            last_status = Status::PermanentFailure(Error::MtaStsError(format!(
                                "MX {:?} not authorized by policy.",
                                envelope.mx
                            )));
                            continue 'next_host;
                        }
                    } else {
                        trc::event!(
                            MtaSts(MtaStsEvent::Authorized),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Hostname = envelope.mx.to_string(),
                            Details = mta_sts_policy
                                .mx
                                .iter()
                                .map(|mx| trc::Value::String(mx.to_compact_string()))
                                .collect::<Vec<_>>(),
                            Strict = strict,
                        );
                    }
                }

                // Obtain source and remote IPs
                let time = Instant::now();
                let resolve_result = match server
                    .resolve_host(remote_host, &envelope, max_multihomed, message.span_id)
                    .await
                {
                    Ok(result) => {
                        trc::event!(
                            Delivery(DeliveryEvent::IpLookup),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Hostname = envelope.mx.to_string(),
                            Details = result
                                .remote_ips
                                .iter()
                                .map(|ip| trc::Value::from(*ip))
                                .collect::<Vec<_>>(),
                            Limit = max_multihomed,
                            Elapsed = time.elapsed(),
                        );

                        result
                    }
                    Err(status) => {
                        trc::event!(
                            Delivery(DeliveryEvent::IpLookupFailed),
                            SpanId = message.span_id,
                            Domain = domain.domain.clone(),
                            Hostname = envelope.mx.to_string(),
                            Details = status.to_string(),
                            Elapsed = time.elapsed(),
                        );

                        last_status = status;
                        continue 'next_host;
                    }
                };

                // Update TLS strategy
                tls_strategy.dane = server
                    .eval_if(&queue_config.tls.dane, &envelope, message.span_id)
                    .await
                    .unwrap_or(RequireOptional::Optional);
                tls_strategy.tls = server
                    .eval_if(&queue_config.tls.start, &envelope, message.span_id)
                    .await
                    .unwrap_or(RequireOptional::Optional);

                // Lookup DANE policy
                let dane_policy = if tls_strategy.try_dane() && is_smtp {
                    let time = Instant::now();
                    let strict = tls_strategy.is_dane_required();
                    match server
                        .tlsa_lookup(format!("_25._tcp.{}.", envelope.mx))
                        .await
                    {
                        Ok(Some(tlsa)) => {
                            if tlsa.has_end_entities {
                                trc::event!(
                                    Dane(DaneEvent::TlsaRecordFetch),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    Details = format!("{tlsa:?}"),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );

                                tlsa.into()
                            } else {
                                trc::event!(
                                    Dane(DaneEvent::TlsaRecordInvalid),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    Details = format!("{tlsa:?}"),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );

                                // Report invalid TLSA record
                                if let Some(tls_report) = &tls_report {
                                    server
                                        .schedule_report(TlsEvent {
                                            policy: tlsa.into(),
                                            domain: domain.domain.to_string(),
                                            failure: FailureDetails::new(ResultType::TlsaInvalid)
                                                .with_receiving_mx_hostname(envelope.mx)
                                                .with_failure_reason_code("Invalid TLSA record.")
                                                .into(),
                                            tls_record: tls_report.record.clone(),
                                            interval: tls_report.interval,
                                        })
                                        .await;
                                }

                                if strict {
                                    last_status =
                                        Status::PermanentFailure(Error::DaneError(ErrorDetails {
                                            entity: envelope.mx.into(),
                                            details: "No valid TLSA records were found".into(),
                                        }));
                                    continue 'next_host;
                                }
                                None
                            }
                        }
                        Ok(None) => {
                            trc::event!(
                                Dane(DaneEvent::TlsaRecordNotDnssecSigned),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                                Strict = strict,
                                Elapsed = time.elapsed(),
                            );

                            if strict {
                                // Report DANE required
                                if let Some(tls_report) = &tls_report {
                                    server
                                        .schedule_report(TlsEvent {
                                            policy: PolicyType::Tlsa(None),
                                            domain: domain.domain.to_string(),
                                            failure: FailureDetails::new(ResultType::DaneRequired)
                                                .with_receiving_mx_hostname(envelope.mx)
                                                .with_failure_reason_code(
                                                    "No TLSA DNSSEC records found.",
                                                )
                                                .into(),
                                            tls_record: tls_report.record.clone(),
                                            interval: tls_report.interval,
                                        })
                                        .await;
                                }

                                last_status =
                                    Status::PermanentFailure(Error::DaneError(ErrorDetails {
                                        entity: envelope.mx.into(),
                                        details: "No TLSA DNSSEC records found".into(),
                                    }));
                                continue 'next_host;
                            }
                            None
                        }
                        Err(err) => {
                            let not_found = matches!(&err, mail_auth::Error::DnsRecordNotFound(_));

                            if not_found {
                                trc::event!(
                                    Dane(DaneEvent::TlsaRecordNotFound),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            } else {
                                trc::event!(
                                    Dane(DaneEvent::TlsaRecordFetchError),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    CausedBy = trc::Error::from(err.clone()),
                                    Strict = strict,
                                    Elapsed = time.elapsed(),
                                );
                            }

                            if strict {
                                last_status = if not_found {
                                    // Report DANE required
                                    if let Some(tls_report) = &tls_report {
                                        server
                                            .schedule_report(TlsEvent {
                                                policy: PolicyType::Tlsa(None),
                                                domain: domain.domain.to_string(),
                                                failure: FailureDetails::new(
                                                    ResultType::DaneRequired,
                                                )
                                                .with_receiving_mx_hostname(envelope.mx)
                                                .with_failure_reason_code(
                                                    "No TLSA records found for MX.",
                                                )
                                                .into(),
                                                tls_record: tls_report.record.clone(),
                                                interval: tls_report.interval,
                                            })
                                            .await;
                                    }

                                    Status::PermanentFailure(Error::DaneError(ErrorDetails {
                                        entity: envelope.mx.into(),
                                        details: "No TLSA records found".into(),
                                    }))
                                } else {
                                    err.into()
                                };
                                continue 'next_host;
                            }
                            None
                        }
                    }
                } else {
                    None
                };

                // Try each IP address
                'next_ip: for remote_ip in resolve_result.remote_ips {
                    // Set source IP, if any
                    let source_ip = if remote_ip.is_ipv4() {
                        resolve_result.source_ipv4
                    } else {
                        resolve_result.source_ipv6
                    };
                    envelope.local_ip = source_ip.unwrap_or(no_ip);

                    // Throttle remote host
                    envelope.remote_ip = remote_ip;
                    for throttle in &queue_config.outbound_limiters.remote {
                        if let Err(retry_at) = server
                            .is_allowed(throttle, &envelope, message.span_id)
                            .await
                        {
                            trc::event!(
                                Delivery(DeliveryEvent::RateLimitExceeded),
                                SpanId = message.span_id,
                                Id = throttle.id.clone(),
                                RemoteIp = remote_ip,
                            );
                            message.domains[domain_idx].set_rate_limiter_error(retry_at);
                            continue 'next_domain;
                        }
                    }

                    // Connect
                    let time = Instant::now();
                    let conn_timeout = server
                        .eval_if(&queue_config.timeout.connect, &envelope, message.span_id)
                        .await
                        .unwrap_or_else(|| Duration::from_secs(5 * 60));
                    let mut smtp_client = match if let Some(ip_addr) = source_ip {
                        SmtpClient::connect_using(
                            ip_addr,
                            SocketAddr::new(remote_ip, remote_host.port()),
                            conn_timeout,
                            span_id,
                        )
                        .await
                    } else {
                        SmtpClient::connect(
                            SocketAddr::new(remote_ip, remote_host.port()),
                            conn_timeout,
                            span_id,
                        )
                        .await
                    } {
                        Ok(smtp_client) => {
                            trc::event!(
                                Delivery(DeliveryEvent::Connect),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                                LocalIp = source_ip.unwrap_or(no_ip),
                                RemoteIp = remote_ip,
                                RemotePort = remote_host.port(),
                                Elapsed = time.elapsed(),
                            );

                            smtp_client
                        }
                        Err(err) => {
                            trc::event!(
                                Delivery(DeliveryEvent::ConnectError),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                                LocalIp = source_ip,
                                RemoteIp = remote_ip,
                                RemotePort = remote_host.port(),
                                CausedBy = from_mail_send_error(&err),
                                Elapsed = time.elapsed(),
                            );

                            last_status = Status::from_smtp_error(envelope.mx, "", err);
                            continue 'next_ip;
                        }
                    };

                    // Obtain session parameters
                    let local_hostname = server
                        .eval_if::<String, _>(&queue_config.hostname, &envelope, message.span_id)
                        .await
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            trc::event!(
                                Delivery(DeliveryEvent::MissingOutboundHostname),
                                SpanId = message.span_id,
                            );
                            "local.host".into()
                        });
                    let params = SessionParams {
                        session_id: message.span_id,
                        server: &server,
                        credentials: remote_host.credentials(),
                        is_smtp: remote_host.is_smtp(),
                        hostname: envelope.mx,
                        local_hostname: &local_hostname,
                        timeout_ehlo: server
                            .eval_if(&queue_config.timeout.ehlo, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60)),
                        timeout_mail: server
                            .eval_if(&queue_config.timeout.mail, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60)),
                        timeout_rcpt: server
                            .eval_if(&queue_config.timeout.rcpt, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60)),
                        timeout_data: server
                            .eval_if(&queue_config.timeout.data, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60)),
                    };

                    // Prepare TLS connector
                    let is_strict_tls = tls_strategy.is_tls_required()
                        || (message.flags & MAIL_REQUIRETLS) != 0
                        || mta_sts_policy.is_some()
                        || dane_policy.is_some();
                    // As per RFC7671 Section 5.1, DANE-EE(3) allows name mismatch
                    let tls_connector = if allow_invalid_certs
                        || remote_host.allow_invalid_certs()
                        || dane_policy.as_ref().is_some_and(|t| t.has_end_entities)
                    {
                        &server.inner.data.smtp_connectors.dummy_verify
                    } else {
                        &server.inner.data.smtp_connectors.pki_verify
                    };

                    let delivery_result = if !remote_host.implicit_tls() {
                        // Read greeting
                        smtp_client.timeout = server
                            .eval_if(&queue_config.timeout.greeting, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60));
                        if let Err(status) = smtp_client.read_greeting(envelope.mx).await {
                            trc::event!(
                                Delivery(DeliveryEvent::GreetingFailed),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                                Details = status.to_string(),
                            );

                            last_status = status;
                            continue 'next_host;
                        }

                        // Say EHLO
                        let time = Instant::now();
                        let capabilities = match smtp_client.say_helo(&params).await {
                            Ok(capabilities) => {
                                trc::event!(
                                    Delivery(DeliveryEvent::Ehlo),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    Details = capabilities.capabilities(),
                                    Elapsed = time.elapsed(),
                                );

                                capabilities
                            }
                            Err(status) => {
                                trc::event!(
                                    Delivery(DeliveryEvent::EhloRejected),
                                    SpanId = message.span_id,
                                    Domain = domain.domain.clone(),
                                    Hostname = envelope.mx.to_string(),
                                    Details = status.to_string(),
                                    Elapsed = time.elapsed(),
                                );

                                last_status = status;
                                continue 'next_host;
                            }
                        };

                        // Try starting TLS
                        if tls_strategy.try_start_tls() {
                            let time = Instant::now();
                            smtp_client.timeout = server
                                .eval_if(&queue_config.timeout.tls, &envelope, message.span_id)
                                .await
                                .unwrap_or_else(|| Duration::from_secs(3 * 60));
                            match smtp_client
                                .try_start_tls(tls_connector, envelope.mx, &capabilities)
                                .await
                            {
                                StartTlsResult::Success { smtp_client } => {
                                    trc::event!(
                                        Delivery(DeliveryEvent::StartTls),
                                        SpanId = message.span_id,
                                        Domain = domain.domain.clone(),
                                        Hostname = envelope.mx.to_string(),
                                        Version = format!(
                                            "{:?}",
                                            smtp_client
                                                .tls_connection()
                                                .protocol_version()
                                                .unwrap()
                                        ),
                                        Details = format!(
                                            "{:?}",
                                            smtp_client
                                                .tls_connection()
                                                .negotiated_cipher_suite()
                                                .unwrap()
                                        ),
                                        Elapsed = time.elapsed(),
                                    );

                                    // Verify DANE
                                    if let Some(dane_policy) = &dane_policy {
                                        if let Err(status) = dane_policy.verify(
                                            message.span_id,
                                            envelope.mx,
                                            smtp_client.tls_connection().peer_certificates(),
                                        ) {
                                            // Report DANE verification failure
                                            if let Some(tls_report) = &tls_report {
                                                server
                                                    .schedule_report(TlsEvent {
                                                        policy: dane_policy.into(),
                                                        domain: domain.domain.to_string(),
                                                        failure: FailureDetails::new(
                                                            ResultType::ValidationFailure,
                                                        )
                                                        .with_receiving_mx_hostname(envelope.mx)
                                                        .with_receiving_ip(remote_ip)
                                                        .with_failure_reason_code(
                                                            "No matching certificates found.",
                                                        )
                                                        .into(),
                                                        tls_record: tls_report.record.clone(),
                                                        interval: tls_report.interval,
                                                    })
                                                    .await;
                                            }

                                            last_status = status;
                                            continue 'next_host;
                                        }
                                    }

                                    // Report TLS success
                                    if let Some(tls_report) = &tls_report {
                                        server
                                            .schedule_report(TlsEvent {
                                                policy: (&mta_sts_policy, &dane_policy).into(),
                                                domain: domain.domain.to_string(),
                                                failure: None,
                                                tls_record: tls_report.record.clone(),
                                                interval: tls_report.interval,
                                            })
                                            .await;
                                    }

                                    // Deliver message over TLS
                                    message
                                        .deliver(
                                            smtp_client,
                                            recipients
                                                .iter_mut()
                                                .filter(|r| r.domain_idx == domain_idx as u32),
                                            params,
                                        )
                                        .await
                                }
                                StartTlsResult::Unavailable {
                                    response,
                                    smtp_client,
                                } => {
                                    // Report unavailable STARTTLS
                                    let reason =
                                        response.as_ref().map(|r| r.to_string()).unwrap_or_else(
                                            || "STARTTLS was not advertised by host".to_string(),
                                        );

                                    trc::event!(
                                        Delivery(DeliveryEvent::StartTlsUnavailable),
                                        SpanId = message.span_id,
                                        Domain = domain.domain.clone(),
                                        Hostname = envelope.mx.to_string(),
                                        Code = response.as_ref().map(|r| r.code()),
                                        Details = response
                                            .as_ref()
                                            .map(|r| r.message().as_str())
                                            .unwrap_or("STARTTLS was not advertised by host")
                                            .to_string(),
                                        Elapsed = time.elapsed(),
                                    );

                                    if let Some(tls_report) = &tls_report {
                                        server
                                            .schedule_report(TlsEvent {
                                                policy: (&mta_sts_policy, &dane_policy).into(),
                                                domain: domain.domain.to_string(),
                                                failure: FailureDetails::new(
                                                    ResultType::StartTlsNotSupported,
                                                )
                                                .with_receiving_mx_hostname(envelope.mx)
                                                .with_receiving_ip(remote_ip)
                                                .with_failure_reason_code(reason)
                                                .into(),
                                                tls_record: tls_report.record.clone(),
                                                interval: tls_report.interval,
                                            })
                                            .await;
                                    }

                                    if is_strict_tls {
                                        last_status =
                                            Status::from_starttls_error(envelope.mx, response);
                                        continue 'next_host;
                                    } else {
                                        // TLS is not required, proceed in plain-text
                                        message
                                            .deliver(
                                                smtp_client,
                                                recipients
                                                    .iter_mut()
                                                    .filter(|r| r.domain_idx == domain_idx as u32),
                                                params,
                                            )
                                            .await
                                    }
                                }
                                StartTlsResult::Error { error } => {
                                    trc::event!(
                                        Delivery(DeliveryEvent::StartTlsError),
                                        SpanId = message.span_id,
                                        Domain = domain.domain.clone(),
                                        Hostname = envelope.mx.to_string(),
                                        Reason = from_mail_send_error(&error),
                                        Elapsed = time.elapsed(),
                                    );

                                    // Report TLS failure
                                    if let (Some(tls_report), mail_send::Error::Tls(error)) =
                                        (&tls_report, &error)
                                    {
                                        server
                                            .schedule_report(TlsEvent {
                                                policy: (&mta_sts_policy, &dane_policy).into(),
                                                domain: domain.domain.to_string(),
                                                failure: FailureDetails::new(
                                                    ResultType::CertificateNotTrusted,
                                                )
                                                .with_receiving_mx_hostname(envelope.mx)
                                                .with_receiving_ip(remote_ip)
                                                .with_failure_reason_code(error.to_string())
                                                .into(),
                                                tls_record: tls_report.record.clone(),
                                                interval: tls_report.interval,
                                            })
                                            .await;
                                    }

                                    last_status = if is_strict_tls {
                                        Status::from_tls_error(envelope.mx, error)
                                    } else {
                                        Status::from_tls_error(envelope.mx, error).into_temporary()
                                    };
                                    continue 'next_host;
                                }
                            }
                        } else {
                            // TLS has been disabled
                            trc::event!(
                                Delivery(DeliveryEvent::StartTlsDisabled),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                            );

                            message
                                .deliver(
                                    smtp_client,
                                    recipients
                                        .iter_mut()
                                        .filter(|r| r.domain_idx == domain_idx as u32),
                                    params,
                                )
                                .await
                        }
                    } else {
                        // Start TLS
                        smtp_client.timeout = server
                            .eval_if(&queue_config.timeout.tls, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(3 * 60));
                        let mut smtp_client =
                            match smtp_client.into_tls(tls_connector, envelope.mx).await {
                                Ok(smtp_client) => smtp_client,
                                Err(error) => {
                                    trc::event!(
                                        Delivery(DeliveryEvent::ImplicitTlsError),
                                        SpanId = message.span_id,
                                        Domain = domain.domain.clone(),
                                        Hostname = envelope.mx.to_string(),
                                        Reason = from_mail_send_error(&error),
                                    );

                                    last_status = Status::from_tls_error(envelope.mx, error);
                                    continue 'next_host;
                                }
                            };

                        // Read greeting
                        smtp_client.timeout = server
                            .eval_if(&queue_config.timeout.greeting, &envelope, message.span_id)
                            .await
                            .unwrap_or_else(|| Duration::from_secs(5 * 60));
                        if let Err(status) = smtp_client.read_greeting(envelope.mx).await {
                            trc::event!(
                                Delivery(DeliveryEvent::GreetingFailed),
                                SpanId = message.span_id,
                                Domain = domain.domain.clone(),
                                Hostname = envelope.mx.to_string(),
                                Details = from_error_status(&status),
                            );

                            last_status = status;
                            continue 'next_host;
                        }

                        // Deliver message
                        message
                            .deliver(
                                smtp_client,
                                recipients
                                    .iter_mut()
                                    .filter(|r| r.domain_idx == domain_idx as u32),
                                params,
                            )
                            .await
                    };

                    // Update status for the current domain and continue with the next one
                    let schedule = server
                        .eval_if::<Vec<Duration>, _>(
                            &queue_config.retry,
                            &envelope,
                            message.span_id,
                        )
                        .await
                        .unwrap_or_else(|| vec![Duration::from_secs(60)]);
                    message.domains[domain_idx].set_status(delivery_result, &schedule);
                    continue 'next_domain;
                }
            }

            // Update status
            let schedule = server
                .eval_if::<Vec<Duration>, _>(&queue_config.retry, &envelope, message.span_id)
                .await
                .unwrap_or_else(|| vec![Duration::from_secs(60)]);
            message.domains[domain_idx].set_status(last_status, &schedule);
        }
        message.recipients = recipients;

        // Send Delivery Status Notifications
        server.send_dsn(&mut message).await;

        // Notify queue manager
        if let Some(due) = message.next_event() {
            trc::event!(
                Queue(trc::QueueEvent::Rescheduled),
                SpanId = span_id,
                NextRetry = trc::Value::Timestamp(message.next_delivery_event()),
                NextDsn = trc::Value::Timestamp(message.next_dsn()),
                Expires = trc::Value::Timestamp(message.expires()),
            );

            // Save changes to disk
            message
                .save_changes(&server, self.due.into(), due.into())
                .await;

            QueueEventStatus::Deferred
        } else {
            trc::event!(
                Delivery(DeliveryEvent::Completed),
                SpanId = span_id,
                Elapsed = trc::Value::Duration((now() - message.created) * 1000)
            );

            // Delete message from queue
            message.remove(&server, self.due).await;

            QueueEventStatus::Completed
        }
    }
}

impl Message {
    /// Marks as failed all domains that reached their expiration time
    pub fn has_pending_delivery(&mut self) -> bool {
        let now = now();
        let mut has_pending_delivery = false;

        for (idx, domain) in self.domains.iter_mut().enumerate() {
            match &domain.status {
                Status::TemporaryFailure(_) if domain.expires <= now => {
                    trc::event!(
                        Delivery(DeliveryEvent::Failed),
                        SpanId = self.span_id,
                        Domain = domain.domain.clone(),
                        Reason = from_error_status(&domain.status),
                        Details = trc::Value::Timestamp(now),
                        Expires = trc::Value::Timestamp(domain.expires),
                        NextRetry = trc::Value::Timestamp(domain.retry.due),
                        NextDsn = trc::Value::Timestamp(domain.notify.due),
                    );

                    for rcpt in &mut self.recipients {
                        if rcpt.domain_idx == idx as u32 {
                            rcpt.status = std::mem::replace(&mut rcpt.status, Status::Scheduled)
                                .into_permanent();
                        }
                    }

                    domain.status =
                        std::mem::replace(&mut domain.status, Status::Scheduled).into_permanent();
                }
                Status::Scheduled if domain.expires <= now => {
                    let mut had_attempts = false;
                    for rcpt in &mut self.recipients {
                        if rcpt.domain_idx == idx as u32 {
                            had_attempts |= !matches!(rcpt.status, Status::Scheduled);
                            rcpt.status = std::mem::replace(&mut rcpt.status, Status::Scheduled)
                                .into_permanent();
                        }
                    }

                    let reason = if had_attempts {
                        "Message delivery failed."
                    } else {
                        "Message expired without any delivery attempts made."
                    };

                    trc::event!(
                        Delivery(DeliveryEvent::Failed),
                        SpanId = self.span_id,
                        Domain = domain.domain.clone(),
                        Reason = reason,
                        Details = trc::Value::Timestamp(now),
                        Expires = trc::Value::Timestamp(domain.expires),
                        NextRetry = trc::Value::Timestamp(domain.retry.due),
                        NextDsn = trc::Value::Timestamp(domain.notify.due),
                    );

                    domain.status = Status::PermanentFailure(Error::Io(reason.into()));
                }
                Status::Completed(_) | Status::PermanentFailure(_) => (),
                _ => {
                    has_pending_delivery = true;
                }
            }
        }

        has_pending_delivery
    }
}

impl Domain {
    pub fn set_status(&mut self, status: impl Into<Status<(), Error>>, schedule: &[Duration]) {
        self.status = status.into();
        if matches!(
            &self.status,
            Status::TemporaryFailure(_) | Status::Scheduled
        ) {
            self.retry(schedule);
        }
    }

    pub fn retry(&mut self, schedule: &[Duration]) {
        self.retry.due = now()
            + schedule[std::cmp::min(self.retry.inner as usize, schedule.len() - 1)].as_secs();
        self.retry.inner += 1;
    }
}
