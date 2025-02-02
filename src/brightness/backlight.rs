use crate::device_file::{read, write};
use dbus::channel::Sender;
use dbus::{self, blocking::Connection, Message};
use inotify::{Inotify, WatchMask};
use std::error::Error;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;

struct Dbus {
    connection: Connection,
    message: Message,
}

pub struct Backlight {
    file: File,
    min_brightness: u64,
    max_brightness: u64,
    inotify: Inotify,
    current: Option<u64>,
    dbus: Option<Dbus>,
    has_write_permission: bool,
}

impl Backlight {
    pub fn new(path: &str, min_brightness: u64) -> Result<Self, Box<dyn Error>> {
        let brightness_path = Path::new(path).join("brightness");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&brightness_path)?;
        let current_brightness = read(&mut file)?;
        let has_write_permission = write(&mut file, current_brightness).is_ok();

        let dbus = if has_write_permission {
            None
        } else {
            let id = Path::new(path)
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or("Unable to identify backlight ID")?;

            let message = Message::new_method_call(
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
                "SetBrightness",
            )
            .ok()
            .map(|m| m.append2("backlight", id));

            Connection::new_system().ok().and_then(|connection| {
                message.map(|message| Dbus {
                    connection,
                    message,
                })
            })
        };

        let max_brightness = fs::read_to_string(Path::new(path).join("max_brightness"))?
            .trim()
            .parse()?;

        let mut inotify = Inotify::init()?;
        inotify.add_watch(&brightness_path, WatchMask::MODIFY)?;

        let brightness_hw_changed_path = Path::new(path).join("brightness_hw_changed");
        if Path::new(&brightness_hw_changed_path).exists() {
            inotify.add_watch(&brightness_hw_changed_path, WatchMask::MODIFY)?;
        }

        Ok(Self {
            file,
            min_brightness,
            max_brightness,
            inotify,
            current: None,
            dbus,
            has_write_permission,
        })
    }
}

impl super::Brightness for Backlight {
    fn get(&mut self) -> Result<u64, Box<dyn Error>> {
        let update = |this: &mut Self| {
            let value = read(&mut this.file)? as u64;
            this.current = Some(value);
            Ok(value)
        };

        let mut buffer = [0u8; 1024];
        match (self.inotify.read_events(&mut buffer), self.current) {
            (_, None) => update(self),
            (Ok(mut events), Some(cached)) => {
                if events.next().is_some() {
                    update(self)
                } else {
                    Ok(cached)
                }
            }
            (Err(err), Some(cached)) if err.kind() == ErrorKind::WouldBlock => Ok(cached),
            (Err(err), _) => Err(err.into()),
        }
    }

    fn set(&mut self, value: u64) -> Result<u64, Box<dyn Error>> {
        let value = value.clamp(self.min_brightness, self.max_brightness);

        if self.has_write_permission {
            write(&mut self.file, value as f64)?;
        } else if let Some(dbus) = &self.dbus {
            dbus.connection
                .send(dbus.message.duplicate()?.append1(value as u32))
                .map_err(|_| "Unable to send brightness change message via dbus")?;
        } else {
            Err(std::io::Error::from(ErrorKind::PermissionDenied))?
        }

        self.current = Some(value);

        // Consume file events to not trigger get() update
        let mut buffer = [0u8; 1024];
        match self.inotify.read_events(&mut buffer) {
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(value),
            Err(err) => Err(err.into()),
            _ => Ok(value),
        }
    }
}
