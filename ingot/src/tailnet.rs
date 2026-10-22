//! Whether this node's copper subnet is live on the tailnet as a subnet
//! route, so tailnet devices reach its VMs directly. Read-only: ingot never
//! changes tailscaled's settings; advertising the route is the owner's call.

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct RouteCheck {
    last: Mutex<Option<(Instant, String, Option<bool>)>>,
}

impl RouteCheck {
    /// `Some(true)` once `subnet` is one of this node's primary routes:
    /// advertised and approved. `None` when Tailscale can't be asked.
    pub fn live(&self, subnet: &str) -> Option<bool> {
        let mut last = self.last.lock().unwrap();
        if let Some((at, asked, answer)) = last.as_ref() {
            if asked == subnet && at.elapsed() < EVERY {
                return *answer;
            }
        }
        let answer = Command::new("tailscale")
            .args(["status", "--json", "--peers=false"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| primary(&o.stdout, subnet));
        *last = Some((Instant::now(), subnet.to_string(), answer));
        answer
    }
}

fn primary(status: &[u8], subnet: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_slice(status).ok()?;
    let routes = v.get("Self")?.get("PrimaryRoutes").and_then(|r| r.as_array());
    Some(routes.is_some_and(|r| r.iter().any(|r| r.as_str() == Some(subnet))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_is_live_once_it_is_primary() {
        let none = br#"{"Self": {"PrimaryRoutes": null}}"#;
        let live = br#"{"Self": {"PrimaryRoutes": ["10.42.1.0/24"]}}"#;
        assert_eq!(primary(none, "10.42.1.0/24"), Some(false));
        assert_eq!(primary(live, "10.42.1.0/24"), Some(true));
        assert_eq!(primary(live, "10.42.2.0/24"), Some(false));
        assert_eq!(primary(b"not json", "10.42.1.0/24"), None);
    }
}
