use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatusKind {
    #[default]
    Idle,
    Running,
    Waiting,
    Done,
}

impl AgentStatusKind {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Idle => "·",
            Self::Running => "…",
            Self::Waiting => "●",
            Self::Done => "✓",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "",
            Self::Running => "run",
            Self::Waiting => "wait",
            Self::Done => "done",
        }
    }

    pub fn is_attention(self) -> bool {
        matches!(self, Self::Waiting | Self::Done)
    }

    pub fn priority(self) -> usize {
        match self {
            Self::Waiting => 3,
            Self::Running => 2,
            Self::Done => 1,
            Self::Idle => 0,
        }
    }
}
