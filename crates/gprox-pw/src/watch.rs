//! Watch the state of device nodes by name (main-loop thread only).
//!
//! gproxd uses it to see when the headset's own capture or playback stream
//! starts or stops, whichever client caused it: the dongle sends a stray
//! mute-button report at those moments, which must not count as a press.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use pipewire as pw;
use pw::core::CoreRc;
use pw::node::{Node, NodeChangeMask, NodeListener, NodeState};
use pw::registry::{Listener, RegistryRc};
use pw::types::ObjectType;

use crate::Error;

/// A node's state, without the error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Creating,
    Suspended,
    Idle,
    Running,
    Error,
}

impl From<&NodeState<'_>> for DeviceState {
    fn from(s: &NodeState<'_>) -> Self {
        match s {
            NodeState::Creating => DeviceState::Creating,
            NodeState::Suspended => DeviceState::Suspended,
            NodeState::Idle => DeviceState::Idle,
            NodeState::Running => DeviceState::Running,
            NodeState::Error(_) => DeviceState::Error,
        }
    }
}

/// A device's stream starting or stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEdge {
    /// The device started running (from suspended or idle).
    Started,
    /// The device was suspended (closed) after it had been opened.
    Stopped,
}

/// What a state change means for the device's stream. The first state seen
/// (`old == None`, e.g. at gproxd start or when the device appears) is not
/// an edge: it tells nothing about when the state began.
pub fn stream_edge(old: Option<DeviceState>, new: DeviceState) -> Option<StreamEdge> {
    use DeviceState::*;
    match (old?, new) {
        (Suspended | Idle, Running) => Some(StreamEdge::Started),
        (Idle | Running, Suspended) => Some(StreamEdge::Stopped),
        _ => None,
    }
}

/// Called with the node name and the edge.
pub type OnEdge = Rc<dyn Fn(&str, StreamEdge)>;

struct Watched {
    _listener: NodeListener,
    _node: Node,
}

/// Watches every node whose `node.name` is in a list, including nodes that
/// appear later (a re-plugged device). Dropping it stops watching.
pub struct NodeWatch {
    _listener: Listener,
    _registry: RegistryRc,
    _nodes: Rc<RefCell<HashMap<u32, Watched>>>,
}

impl std::fmt::Debug for NodeWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeWatch").finish_non_exhaustive()
    }
}

impl NodeWatch {
    pub fn new(core: &CoreRc, names: Vec<String>, on_edge: OnEdge) -> Result<Self, Error> {
        let registry = core.get_registry_rc()?;
        let nodes: Rc<RefCell<HashMap<u32, Watched>>> = Rc::default();
        let weak = registry.downgrade();
        let listener = registry
            .add_listener_local()
            .global({
                let nodes = nodes.clone();
                move |g| {
                    if g.type_ != ObjectType::Node {
                        return;
                    }
                    let Some(name) = g
                        .props
                        .and_then(|p| p.get(*pw::keys::NODE_NAME))
                        .filter(|n| names.iter().any(|w| w == n))
                        .map(str::to_owned)
                    else {
                        return;
                    };
                    let Some(registry) = weak.upgrade() else {
                        return;
                    };
                    let node: Node = match registry.bind(g) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!(node = %name, error = %e, "cannot watch node");
                            return;
                        }
                    };
                    let last: Cell<Option<DeviceState>> = Cell::new(None);
                    let on_edge = on_edge.clone();
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            if !info.change_mask().contains(NodeChangeMask::STATE)
                                && last.get().is_some()
                            {
                                return;
                            }
                            let new = DeviceState::from(&info.state());
                            let old = last.replace(Some(new));
                            if old != Some(new) {
                                tracing::debug!(node = %name, ?old, ?new, "device state");
                            }
                            if let Some(edge) = stream_edge(old, new) {
                                on_edge(&name, edge);
                            }
                        })
                        .register();
                    nodes.borrow_mut().insert(
                        g.id,
                        Watched {
                            _listener: listener,
                            _node: node,
                        },
                    );
                }
            })
            .global_remove({
                let nodes = nodes.clone();
                move |id| {
                    nodes.borrow_mut().remove(&id);
                }
            })
            .register();
        Ok(NodeWatch {
            _listener: listener,
            _registry: registry,
            _nodes: nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeviceState::*;

    #[test]
    fn edges() {
        assert_eq!(
            stream_edge(Some(Suspended), Running),
            Some(StreamEdge::Started)
        );
        assert_eq!(stream_edge(Some(Idle), Running), Some(StreamEdge::Started));
        // Seen live: running → idle at once, suspended 5 s
        // later; the dongle's report comes with the suspend.
        assert_eq!(stream_edge(Some(Running), Idle), None);
        assert_eq!(
            stream_edge(Some(Idle), Suspended),
            Some(StreamEdge::Stopped)
        );
        assert_eq!(
            stream_edge(Some(Running), Suspended),
            Some(StreamEdge::Stopped)
        );
        // The first state seen is not an edge, nor is a repeat or a device
        // that is still being created or failed.
        for s in [Creating, Suspended, Idle, Running, Error] {
            assert_eq!(stream_edge(None, s), None);
            assert_eq!(stream_edge(Some(s), s), None);
        }
        assert_eq!(stream_edge(Some(Creating), Running), None);
        assert_eq!(stream_edge(Some(Creating), Suspended), None);
        assert_eq!(stream_edge(Some(Error), Suspended), None);
    }
}
