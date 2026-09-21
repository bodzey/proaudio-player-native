mod backend;
mod meters;
mod network_audio;
mod webui;

pub use backend::serve;
pub use backend::WebController as ApiController;

// Transitional internal name kept for protocol-compatibility modules on dev.
// New composition code should use ApiController.
pub(crate) use backend::WebController;
