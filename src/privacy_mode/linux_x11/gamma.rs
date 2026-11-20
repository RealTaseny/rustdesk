use hbb_common::{log, ResultType};
use serde_derive::{Deserialize, Serialize};
use std::collections::BTreeMap;
use x11rb::{
    connection::Connection,
    protocol::{
        randr::{ConnectionExt as _, Crtc, GetCrtcGammaReply},
        xproto::{Atom, AtomEnum, ConnectionExt as _, PropMode, Window},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    NONE,
};

const BACKUP_LIMIT: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Ramp {
    red: Vec<u16>,
    green: Vec<u16>,
    blue: Vec<u16>,
}

#[derive(Serialize, Deserialize)]
struct SavedGammas {
    session: String,
    ramps: BTreeMap<Crtc, Ramp>,
}

pub(super) struct GammaBackup {
    root: Window,
    atom: Atom,
    saved: SavedGammas,
}

impl GammaBackup {
    pub(super) fn load(x11: &RustConnection, session: &str) -> ResultType<Self> {
        let root = x11.setup().roots[0].root;
        let atom = x11
            .intern_atom(false, b"_RUSTDESK_X11_PRIVACY_GAMMA_V1")?
            .reply()?
            .atom;
        let property = x11
            .get_property(
                false,
                root,
                atom,
                AtomEnum::STRING,
                0,
                (BACKUP_LIMIT / 4) as u32,
            )?
            .reply()?;
        let saved = if property.type_ == NONE {
            SavedGammas {
                session: session.to_owned(),
                ramps: BTreeMap::new(),
            }
        } else {
            if property.format != 8 || property.bytes_after != 0 {
                hbb_common::bail!("Invalid X11 privacy color backup property");
            }
            let saved: SavedGammas = serde_json::from_slice(&property.value)?;
            if saved.session != session
                || saved.ramps.values().any(|ramp| {
                    ramp.red.is_empty()
                        || ramp.red.len() > u16::MAX as usize
                        || ramp.red.len() != ramp.green.len()
                        || ramp.red.len() != ramp.blue.len()
                })
            {
                hbb_common::bail!("The X11 privacy color backup does not match this session");
            }
            saved
        };
        Ok(Self { root, atom, saved })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.saved.ramps.is_empty()
    }

    pub(super) fn remember(
        &mut self,
        x11: &RustConnection,
        crtc: Crtc,
        gamma: GetCrtcGammaReply,
    ) -> ResultType<()> {
        if self.saved.ramps.contains_key(&crtc) {
            return Ok(());
        }
        self.saved.ramps.insert(
            crtc,
            Ramp {
                red: gamma.red,
                green: gamma.green,
                blue: gamma.blue,
            },
        );
        // The root window outlives the helper but is cleared when this X server resets.
        if let Err(error) = self.write(x11) {
            self.saved.ramps.remove(&crtc);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn restore(mut self, x11: &RustConnection, available: &[Crtc]) -> ResultType<()> {
        self.saved.ramps.retain(|&crtc, ramp| {
            if !available.contains(&crtc) {
                return false;
            }
            let result: ResultType<()> = (|| {
                x11.randr_set_crtc_gamma(crtc, &ramp.red, &ramp.green, &ramp.blue)?
                    .check()?;
                Ok(())
            })();
            if let Err(error) = result {
                log::warn!("X11 privacy screen could not restore CRTC {crtc}: {error:#}");
                return true;
            }
            false
        });
        self.write(x11)?;
        if !self.saved.ramps.is_empty() {
            hbb_common::bail!("Some X11 privacy color tables remain backed up");
        }
        Ok(())
    }

    fn write(&self, x11: &RustConnection) -> ResultType<()> {
        if self.saved.ramps.is_empty() {
            x11.delete_property(self.root, self.atom)?.check()?;
        } else {
            let encoded = serde_json::to_vec(&self.saved)?;
            if encoded.len() > BACKUP_LIMIT {
                hbb_common::bail!("The X11 privacy color backup is too large");
            }
            x11.change_property8(
                PropMode::REPLACE,
                self.root,
                self.atom,
                AtomEnum::STRING,
                &encoded,
            )?
            .check()?;
        }
        Ok(())
    }
}
