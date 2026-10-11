//! GloorpFlorp's app logic without the window: the engine that wires the
//! network session, video and audio together, the settings file and logging.
//! The binary (`main.rs`, `ui.rs`) adds the egui window on top.

pub mod engine;
pub mod logging;
pub mod settings;

pub use engine::{Command, Engine, EngineOptions, Phase, ShareInfo, ShareState, Snapshot};
pub use settings::Settings;
