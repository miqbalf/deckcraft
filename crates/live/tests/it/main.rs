//! The one integration test binary of `deckcraft-live`; modules sit beside this file.
mod doc;
mod free;
mod gate;
mod hold;
mod order;
mod presence;
mod reconnect;
#[cfg(feature = "native-hub")]
mod relay;
mod support;
mod text;
mod tomb;
mod trace;
mod undo;
mod wire;
