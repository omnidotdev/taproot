use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A change operation type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

/// A parsed NOTIFY payload representing a table change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeNotification {
    pub table: String,
    pub op: ChangeOp,
    pub id: String,
}

impl ChangeNotification {
    /// Parse a NOTIFY payload JSON string
    #[must_use]
    pub fn from_payload(payload: &str) -> Option<Self> {
        serde_json::from_str(payload).ok()
    }
}

/// Broadcast change notifications to all subscribers
#[derive(Debug, Clone)]
pub struct ChangeBroadcaster {
    tx: broadcast::Sender<ChangeNotification>,
}

impl ChangeBroadcaster {
    /// Create a new broadcaster with the given channel capacity
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to change notifications
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ChangeNotification> {
        self.tx.subscribe()
    }

    /// Send a change notification to all subscribers
    pub fn send(&self, notification: ChangeNotification) {
        let _ = self.tx.send(notification);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_notification() {
        let payload = r#"{"table":"users","op":"INSERT","id":"abc-123"}"#;
        let notif = ChangeNotification::from_payload(payload);
        assert!(notif.is_some());
        let notif = notif.unwrap();
        assert_eq!(notif.table, "users");
        assert_eq!(notif.op, ChangeOp::Insert);
        assert_eq!(notif.id, "abc-123");
    }

    #[test]
    fn parse_update_notification() {
        let payload = r#"{"table":"posts","op":"UPDATE","id":"post-1"}"#;
        let notif = ChangeNotification::from_payload(payload).unwrap();
        assert_eq!(notif.op, ChangeOp::Update);
    }

    #[test]
    fn parse_delete_notification() {
        let payload = r#"{"table":"posts","op":"DELETE","id":"post-1"}"#;
        let notif = ChangeNotification::from_payload(payload).unwrap();
        assert_eq!(notif.op, ChangeOp::Delete);
    }

    #[test]
    fn parse_invalid_notification() {
        assert!(ChangeNotification::from_payload("not json").is_none());
        assert!(ChangeNotification::from_payload(r#"{"table":"users"}"#).is_none());
    }

    #[test]
    fn broadcaster_sends_to_receivers() {
        let broadcaster = ChangeBroadcaster::new(64);
        let mut rx = broadcaster.subscribe();
        let notif = ChangeNotification {
            table: "users".into(),
            op: ChangeOp::Insert,
            id: "user-1".into(),
        };
        broadcaster.send(notif);
        let received = rx.try_recv().unwrap();
        assert_eq!(received.table, "users");
        assert_eq!(received.id, "user-1");
    }
}
