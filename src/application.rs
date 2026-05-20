//! kb Abscissa Application

use crate::{commands::EntryPoint, config::KbConfig};
use abscissa_core::{
    Application, FrameworkError, StandardPaths,
    application::{self, AppCell},
    config::{self, CfgCell},
    trace,
};

/// Application state
pub static APP: AppCell<KbApp> = AppCell::new();

/// kb Application
#[derive(Debug)]
pub struct KbApp {
    /// Application configuration.
    config: CfgCell<KbConfig>,

    /// Application state.
    state: application::State<Self>,
}

/// Initialize a new application instance.
impl Default for KbApp {
    fn default() -> Self {
        Self {
            config: CfgCell::default(),
            state: application::State::default(),
        }
    }
}

impl Application for KbApp {
    /// Entrypoint command for this application.
    type Cmd = EntryPoint;

    /// Application configuration.
    type Cfg = KbConfig;

    /// Paths to resources within the application.
    type Paths = StandardPaths;

    /// Accessor for application configuration.
    fn config(&self) -> config::Reader<KbConfig> {
        self.config.read()
    }

    /// Borrow the application state immutably.
    fn state(&self) -> &application::State<Self> {
        &self.state
    }

    /// Register all components used by this application.
    fn register_components(&mut self, command: &Self::Cmd) -> Result<(), FrameworkError> {
        let framework_components = self.framework_components(command)?;
        let mut app_components = self.state.components_mut();
        app_components.register(framework_components)
    }

    /// Post-configuration lifecycle callback.
    fn after_config(&mut self, config: Self::Cfg) -> Result<(), FrameworkError> {
        let mut components = self.state.components_mut();
        components.after_config(&config)?;
        self.config.set_once(config);
        Ok(())
    }

    /// Get tracing configuration from command-line options
    fn tracing_config(&self, command: &EntryPoint) -> trace::Config {
        if command.verbose {
            trace::Config::verbose()
        } else {
            trace::Config::default()
        }
    }
}
