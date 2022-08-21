use anyhow::{anyhow, Context, Error, Result};
use gettextrs::gettext;

use crate::{help::ResultExt, THREAD_POOL};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    #[default]
    Source,
    Sink,
}

impl Class {
    fn for_str(string: &str) -> Option<Self> {
        match string {
            "Audio/Source" => Some(Self::Source),
            "Audio/Sink" => Some(Self::Sink),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Source => "Audio/Source",
            Self::Sink => "Audio/Sink",
        }
    }
}

pub async fn find_default_name(class: Class) -> Result<String> {
    match THREAD_POOL
        .push_future(move || find_default_name_gst(class))
        .context("Failed to push future to main thread pool")?
        .await
    {
        Ok(res) => Ok(res),
        Err(err) => {
            tracing::warn!("Failed to find default name using gstreamer: {:?}", err);
            tracing::debug!("Manually using libpulse instead");

            let server = pa::Server::connect().await?;
            server.find_default_device_name(class).await
        }
    }
}

fn find_default_name_gst(class: Class) -> Result<String> {
    use gst::prelude::*;

    let device_monitor = gst::DeviceMonitor::new();
    device_monitor.add_filter(Some(class.as_str()), None);

    device_monitor.start().map_err(Error::from).with_help(
        || gettext("Make sure that you have PulseAudio installed in your system."),
        || gettext("Failed to start device monitor"),
    )?;
    let devices = device_monitor.devices();
    device_monitor.stop();

    tracing::debug!("Finding device name for class `{:?}`", class);

    for device in devices {
        let device_class = match Class::for_str(&device.device_class()) {
            Some(device_class) => device_class,
            None => {
                tracing::debug!(
                    "Skipping device `{}` as it has unknown device class `{}`",
                    device.name(),
                    device.device_class()
                );
                continue;
            }
        };

        if device_class != class {
            continue;
        }

        let properties = match device.properties() {
            Some(properties) => properties,
            None => {
                tracing::warn!(
                    "Skipping device `{}` as it has no properties",
                    device.name()
                );
                continue;
            }
        };

        let is_default = match properties.get::<bool>("is-default") {
            Ok(is_default) => is_default,
            Err(err) => {
                tracing::warn!(
                    "Skipping device `{}` as it has no `is-default` property. {:?}",
                    device.name(),
                    err
                );
                continue;
            }
        };

        if !is_default {
            tracing::debug!(
                "Skipping device `{}` as it is not the default",
                device.name()
            );
            continue;
        }

        let mut node_name = match properties.get::<String>("node.name") {
            Ok(node_name) => node_name,
            Err(err) => {
                tracing::warn!(
                    "Skipping device `{}` as it has no node.name property. {:?}",
                    device.name(),
                    err
                );
                continue;
            }
        };

        if device_class == Class::Sink {
            node_name.push_str(".monitor");
        }

        return Ok(node_name);
    }

    Err(anyhow!("Failed to find a default device"))
}

mod pa {
    use anyhow::{bail, Context as ErrContext, Error, Result};
    use futures_channel::{mpsc, oneshot};
    use futures_util::StreamExt;
    use gettextrs::gettext;
    use pulse::{
        context::{Context, FlagSet, State},
        def::Retval,
        mainloop::api::Mainloop,
        proplist::{properties, Proplist},
    };

    use std::{cell::RefCell, time::Duration};

    use super::Class;
    use crate::{config::APP_ID, help::ResultExt, utils};

    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct Server {
        main_loop: pulse_glib::Mainloop,
        context: Context,
    }

    impl Server {
        pub async fn connect() -> Result<Self> {
            let main_loop =
                pulse_glib::Mainloop::new(None).context("Failed to create pulse Mainloop")?;

            let mut proplist = Proplist::new().unwrap();
            proplist
                .set_str(properties::APPLICATION_ID, APP_ID)
                .unwrap();
            proplist
                .set_str(properties::APPLICATION_NAME, "Kooha")
                .unwrap();

            let mut context = Context::new_with_proplist(&main_loop, APP_ID, &proplist)
                .context("Failed to create pulse Context")?;

            context
                .connect(None, FlagSet::NOFLAGS, None)
                .map_err(Error::from)
                .with_help(
                    || gettext("Make sure that you have PulseAudio installed in your system."),
                    || gettext("Failed to connect to PulseAudio daemon"),
                )?;

            let (mut tx, mut rx) = mpsc::channel(1);

            context.set_state_callback(Some(Box::new(move || {
                let _ = tx.start_send(());
            })));

            tracing::debug!("Waiting for PA server connection");

            while rx.next().await.is_some() {
                match context.get_state() {
                    State::Ready => break,
                    State::Failed => bail!("Received failed state while connecting"),
                    State::Terminated => bail!("Context connection terminated"),
                    _ => {}
                }
            }

            tracing::debug!("PA Server connected");

            Ok(Self { main_loop, context })
        }

        pub async fn find_default_device_name(&self, class: Class) -> Result<String> {
            let (tx, rx) = oneshot::channel();
            let tx = RefCell::new(Some(tx));

            let mut operation = self
                .context
                .introspect()
                .get_server_info(move |server_info| {
                    let tx = if let Some(tx) = tx.take() {
                        tx
                    } else {
                        tracing::error!("Called get_server_info twice!");
                        return;
                    };

                    match class {
                        Class::Source => {
                            let _ = tx.send(
                                server_info
                                    .default_source_name
                                    .as_ref()
                                    .map(|s| s.to_string()),
                            );
                        }
                        Class::Sink => {
                            let _ = tx.send(
                                server_info
                                    .default_sink_name
                                    .as_ref()
                                    .map(|s| format!("{}.monitor", s)),
                            );
                        }
                    }
                });

            let name = match utils::future_timeout(rx, DEFAULT_TIMEOUT).await {
                Ok(name) => name.unwrap().context("Found no default device")?,
                Err(err) => {
                    operation.cancel();
                    bail!("Failed to receive get_server_info result: {:?}", err)
                }
            };

            Ok(name)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.context.set_state_callback(None);
            self.context.disconnect();
            self.main_loop.quit(Retval(0));
        }
    }
}
