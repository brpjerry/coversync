//! Telling systemd how the agent is doing (`sd_notify`): that it's up, and
//! a line of status that `systemctl --user status coversync-agent` and the
//! app's settings page show. Without systemd nothing is sent.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

pub struct Notifier {
    to: Option<(UnixDatagram, SocketAddr)>,
}

impl Notifier {
    /// The service manager that started this process, if one did
    /// (`$NOTIFY_SOCKET`).
    pub fn from_env() -> Self {
        Self::to(std::env::var("NOTIFY_SOCKET").ok().as_deref())
    }

    fn to(address: Option<&str>) -> Self {
        let open = |address: &str| {
            let addr = match address.strip_prefix('@') {
                Some(name) => SocketAddr::from_abstract_name(name).ok()?,
                None => SocketAddr::from_pathname(address).ok()?,
            };
            Some((UnixDatagram::unbound().ok()?, addr))
        };
        Self {
            to: address.filter(|a| !a.is_empty()).and_then(open),
        }
    }

    fn send(&self, state: &str) {
        if let Some((socket, addr)) = &self.to {
            let _ = socket.send_to_addr(state.as_bytes(), addr);
        }
    }

    /// The agent is up.
    pub fn ready(&self) {
        self.send("READY=1");
    }

    /// The agent is going down.
    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    /// What the agent is doing, in a line.
    pub fn status(&self, text: &str) {
        self.send(&format!("STATUS={}", one_line(text)));
    }
}

/// A status is one line: a line break would start another assignment.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn says_it_to_the_socket_it_was_given() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify");
        let manager = UnixDatagram::bind(&path).unwrap();
        let notifier = Notifier::to(path.to_str());
        notifier.ready();
        notifier.status("Syncing:\n3 of 9 files\nREADY=0");
        let mut heard = Vec::new();
        for _ in 0..2 {
            let mut buf = [0u8; 256];
            let n = manager.recv(&mut buf).unwrap();
            heard.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        assert_eq!(heard, ["READY=1", "STATUS=Syncing: 3 of 9 files READY=0"]);
        // No manager: nothing to send to, and no harm.
        Notifier::to(None).ready();
        Notifier::to(Some("")).status("x");
    }
}
