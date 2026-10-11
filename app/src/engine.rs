//! The engine: owns the network session, encoder, viewer and audio, and
//! reacts to their events. Runs on its own thread; the UI talks to it only
//! through `Command`s and reads a `Snapshot`. Never blocks the UI.
