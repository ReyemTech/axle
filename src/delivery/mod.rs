//! Delivery status: the pure rules for stages and PR links. No store, no IO; both stores'
//! guarded writes and the services call these.

pub mod links;
pub mod stages;
