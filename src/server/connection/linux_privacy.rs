use super::Connection;
use crate::{ipc, privacy_mode::PrivacyModeState};
use base::message_proto::{Message, MessageBox};
use hbb_common::log;

impl Connection {
    pub(super) async fn admit_linux_desktop(&mut self) -> bool {
        let tx = self.tx_from_authed.clone();
        let id = self.inner.id;
        match self
            .linux_desktop
            .admit(move || {
                if let Err(error) = tx.send(ipc::Data::PrivacyModeState((
                    id,
                    PrivacyModeState::OffUnknown,
                    String::new(),
                ))) {
                    log::trace!("Could not notify closed connection {id} of privacy screen failure: {error}");
                }
            })
            .await
        {
            Ok(false) => true,
            // Trust can be revoked while the helper is setting up the displays.
            Ok(true) => self.check_trusted_device().await,
            Err(failure) => {
                self.send_login_error(failure.message()).await;
                false
            }
        }
    }

    pub(super) async fn send_linux_privacy_screen_confirmation(&mut self) {
        let mut message = Message::new();
        message.set_message_box(MessageBox {
            msgtype: "custom-nocancel".to_owned(),
            title: "Privacy mode".to_owned(),
            text: "Enter privacy mode".to_owned(),
            ..Default::default()
        });
        self.send(message).await;
    }
}
