//! Protocol and session library for the Oracle ILOM Remote System Console.
//!
//! No GUI code lives here: front ends (the egui viewer, the VNC bridge)
//! drive a [`session`] and draw its frames themselves.

pub mod codec;
pub mod config;
pub mod crypto;
pub mod hid;
pub mod ivtp;
pub mod jnlp;
pub mod keymap;
pub mod known_certs;
pub mod rc4;
pub mod scsi;
pub mod session;
pub mod tls;
pub mod tokend;
pub mod video;
pub mod vmedia;
pub mod web;
