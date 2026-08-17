//! Settings: the application that changes what every workspace looks like.
//!
//! One page for now, the wallpaper. Each installed wallpaper is shown as a
//! preview with its name and a button that makes it the one every agentdesk
//! draws, plus a row for none at all. Choosing writes one small file in the
//! runtime directory; every agentdesk reads it on its clock tick and re-renders
//! its background. No process is told, nothing is broadcast, and the setting is
//! gone at reboot with everything else.
//!
//! Written the way every application is written: a model, a `render`, whole
//! tree every time, hand-written stable ids, no diffing and no ephemeral state.
//! What is different is only what it links, and that is nothing but awproto:
//! this is a first-party application with no more access than a calculator.

use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::display::{self, Event, Surface, escape};
use awproto::settings;

struct Settings {
    /// Every wallpaper installed, as (name, path).
    wallpapers: Vec<(String, String)>,
    /// The current choice, `None` for a plain background.
    current: Option<String>,
    status: String,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut app = Settings {
        wallpapers: settings::wallpapers(),
        current: settings::wallpaper(),
        status: String::new(),
    };
    app.status = format!("{} wallpaper(s) installed", app.wallpapers.len());

    if let Err(err) = surface.render(&app.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => return,
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        if !app.accept(&surface, &event) {
            continue;
        }

        if let Err(err) = surface.render(&app.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Settings {
    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        // A click on a stale tree is a click on a button that may no longer
        // mean what it meant. Discarded, not guessed at.
        if surface.is_stale(event) || event.action != display::ACTION_CLICK {
            return false;
        }

        let choice: Option<Option<String>> = match event.target.as_str() {
            "use-none" => Some(None),
            target => target
                .strip_prefix("use-")
                .and_then(|index| index.parse::<usize>().ok())
                .and_then(|index| self.wallpapers.get(index))
                .map(|(_, path)| Some(path.clone())),
        };
        let Some(choice) = choice else { return false };

        match settings::set_wallpaper(choice.as_deref()) {
            Ok(()) => {
                self.status = match &choice {
                    Some(path) => format!("wallpaper set to {path}"),
                    None => "wallpaper cleared".to_owned(),
                };
                self.current = choice;
            }
            Err(err) => self.status = format!("could not save the setting: {err}"),
        }
        log(&self.status);
        true
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            r#"<window title="Settings" font="sans">
  <vstack gap="sm">
    <text role="heading">Wallpaper</text>
    <text role="caption" color="muted">{status}</text>
    <divider/>
    <scroll grow="true">
      <vstack gap="md">
"#,
            status = escape(&self.status),
        );

        for (index, (name, path)) in self.wallpapers.iter().enumerate() {
            let current = self.current.as_deref() == Some(path.as_str());
            let _ = write!(
                out,
                r#"        <hstack gap="md">
          <image src="{path}" alt="Preview of the wallpaper {name}" fit="cover"/>
          <vstack gap="sm" grow="true">
            <text weight="bold">{name}</text>
            <text role="caption" color="muted">{path}</text>
            <button id="use-{index}" label="{label}"{state}
                    description="Makes {name} the wallpaper of every agentdesk"/>
          </vstack>
        </hstack>
"#,
                path = escape(path),
                name = escape(name),
                label = if current { "In use" } else { "Use" },
                state = if current { r#" disabled="true""# } else { "" },
            );
        }

        let none = self.current.is_none();
        let _ = write!(
            out,
            r#"        <hstack gap="md">
          <vstack gap="sm" grow="true">
            <text weight="bold">None</text>
            <text role="caption" color="muted">A plain background, no picture.</text>
            <button id="use-none" label="{label}"{state}
                    description="Removes the wallpaper from every agentdesk, leaving a plain background"/>
          </vstack>
        </hstack>
      </vstack>
    </scroll>
  </vstack>
</window>
"#,
            label = if none { "In use" } else { "Use" },
            state = if none { r#" disabled="true""# } else { "" },
        );
        out
    }
}

fn log(message: &str) {
    let line = format!("<6>awsettings: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awsettings: {message}");
    }
}
