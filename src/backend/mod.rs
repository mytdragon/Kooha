mod pipeline_builder;
mod recorder;
mod recorder_controller;
mod screencast_portal;
mod settings;
mod timer;

pub use {
    recorder_controller::{RecorderController, RecorderControllerState},
    settings::Settings,
};

use {
    pipeline_builder::PipelineBuilder,
    recorder::{Recorder, RecorderState},
    screencast_portal::ScreencastPortal,
    timer::{Timer, TimerState},
};
