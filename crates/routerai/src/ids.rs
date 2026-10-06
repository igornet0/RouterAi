//! Strong IDs.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident, $prefix:literal) => {
        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            /// Random id with prefix.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }

            /// From existing string.
            pub fn from_string(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Borrow.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::from_string(s)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self::from_string(s)
            }
        }
    };
}

id_type!(
    /// Event id.
    EventId,
    "evt"
);
id_type!(
    /// Handler id.
    HandlerId,
    "hdl"
);
id_type!(
    /// Agent id.
    AgentId,
    "agt"
);
id_type!(
    /// Run id.
    RunId,
    "run"
);
id_type!(
    /// Tool id.
    ToolId,
    "tool"
);
id_type!(
    /// Schedule id.
    ScheduleId,
    "sch"
);
