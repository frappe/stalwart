/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::listener::SessionStream;

use trc::SmtpEvent;

use crate::core::Session;
use std::fmt::Write;

impl<T: SessionStream> Session<T> {
    pub async fn handle_vrfy(&mut self, address: String) -> Result<(), ()> {
        match self
            .server
            .eval_if::<String, _>(
                &self.server.core.smtp.session.rcpt.directory,
                self,
                self.data.session_id,
            )
            .await
            .and_then(|name| self.server.get_directory(&name))
        {
            Some(directory) if self.params.can_vrfy => {
                match self
                    .server
                    .vrfy(directory, &address.to_lowercase(), self.data.session_id)
                    .await
                {
                    Ok(values) if !values.is_empty() => {
                        let mut result = String::with_capacity(32);
                        for (pos, value) in values.iter().enumerate() {
                            let _ = write!(
                                result,
                                "250{}{}\r\n",
                                if pos == values.len() - 1 { " " } else { "-" },
                                value
                            );
                        }

                        trc::event!(
                            Smtp(SmtpEvent::Vrfy),
                            SpanId = self.data.session_id,
                            To = address,
                            Result = values,
                        );

                        self.write(result.as_bytes()).await
                    }
                    Ok(_) => {
                        trc::event!(
                            Smtp(SmtpEvent::VrfyNotFound),
                            SpanId = self.data.session_id,
                            To = address,
                        );

                        self.write(b"550 5.1.2 Address not found.\r\n").await
                    }
                    Err(err) => {
                        let is_not_supported =
                            err.matches(trc::EventType::Store(trc::StoreEvent::NotSupported));

                        trc::error!(err.span_id(self.data.session_id).details("VRFY failed"));

                        if !is_not_supported {
                            self.write(b"252 2.4.3 Unable to verify address at this time.\r\n")
                                .await
                        } else {
                            self.write(b"550 5.1.2 Address not found.\r\n").await
                        }
                    }
                }
            }
            _ => {
                trc::event!(
                    Smtp(SmtpEvent::VrfyDisabled),
                    SpanId = self.data.session_id,
                    To = address,
                );

                self.write(b"252 2.5.1 VRFY is disabled.\r\n").await
            }
        }
    }

    pub async fn handle_expn(&mut self, address: String) -> Result<(), ()> {
        match self
            .server
            .eval_if::<String, _>(
                &self.server.core.smtp.session.rcpt.directory,
                self,
                self.data.session_id,
            )
            .await
            .and_then(|name| self.server.get_directory(&name))
        {
            Some(directory) if self.params.can_expn => {
                match self
                    .server
                    .expn(directory, &address.to_lowercase(), self.data.session_id)
                    .await
                {
                    Ok(values) if !values.is_empty() => {
                        let mut result = String::with_capacity(32);
                        for (pos, value) in values.iter().enumerate() {
                            let _ = write!(
                                result,
                                "250{}{}\r\n",
                                if pos == values.len() - 1 { " " } else { "-" },
                                value
                            );
                        }

                        trc::event!(
                            Smtp(SmtpEvent::Expn),
                            SpanId = self.data.session_id,
                            To = address,
                            Result = values,
                        );

                        self.write(result.as_bytes()).await
                    }
                    Ok(_) => {
                        trc::event!(
                            Smtp(SmtpEvent::ExpnNotFound),
                            SpanId = self.data.session_id,
                            To = address,
                        );

                        self.write(b"550 5.1.2 Mailing list not found.\r\n").await
                    }
                    Err(err) => {
                        let is_not_supported =
                            err.matches(trc::EventType::Store(trc::StoreEvent::NotSupported));

                        trc::error!(err.span_id(self.data.session_id).details("VRFY failed"));

                        if !is_not_supported {
                            self.write(b"252 2.4.3 Unable to expand mailing list at this time.\r\n")
                                .await
                        } else {
                            self.write(b"550 5.1.2 Mailing list not found.\r\n").await
                        }
                    }
                }
            }
            _ => {
                trc::event!(
                    Smtp(SmtpEvent::ExpnDisabled),
                    SpanId = self.data.session_id,
                    To = address,
                );

                self.write(b"252 2.5.1 EXPN is disabled.\r\n").await
            }
        }
    }
}
