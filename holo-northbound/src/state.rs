//
// Copyright (c) The Holo Core Contributors
//
// SPDX-License-Identifier: MIT
//

use std::fmt::Write;

use holo_utils::yang::SchemaNodeExt;
use holo_yang::YANG_CTX;
use tokio::sync::oneshot;
use yang4::data::{DataNodeRef, DataTree};
use yang4::schema::{SchemaNode, SchemaNodeKind};

use crate::error::Error;
use crate::{NbDaemonSender, ProviderBase, YangObject, api};

// Northbound data provider.
pub trait Provider: ProviderBase {
    type ListEntry<'a>: ListEntryKind;
    const YANG_OPS: YangOps<Self>;
}

// Common behavior for all list entries.
pub trait ListEntryKind: std::fmt::Debug + Default {
    // Return the task associated with the child node of this list entry,
    // identified by its corresponding module name.
    fn child_task(&self, _module_name: &str) -> Option<NbDaemonSender> {
        None
    }
}

// Implemented by all auto-generated YANG container structs that hold state
// data.
pub trait YangContainer<'a, P: Provider> {
    fn new(provider: &'a P, list_entry: &P::ListEntry<'a>) -> Option<Self>
    where
        Self: Sized + 'a;
}

// Implemented by all auto-generated YANG list structs that hold state data.
pub trait YangList<'a, P: Provider> {
    const STREAMABLE: bool = false;

    fn iter(
        provider: &'a P,
        list_entry: &P::ListEntry<'a>,
    ) -> Option<ListIterator<'a, P>>;

    fn new(provider: &'a P, list_entry: &P::ListEntry<'a>) -> Self
    where
        Self: Sized + 'a;
}

// Static dispatch tables generated from YANG models.
pub struct YangOps<P: Provider> {
    pub list: phf::Map<&'static str, YangListOps<P>>,
    pub container: phf::Map<&'static str, YangContainerOps<P>>,
}

pub struct YangListOps<P: Provider> {
    pub iter: YangListIterFn<P>,
    pub new: YangListNewFn<P>,
    pub streamable: bool,
}

pub struct YangContainerOps<P: Provider> {
    pub new: YangContainerNewFn<P>,
}

// Filter options for Get requests.
pub(crate) struct GetFilter {
    pub max_depth: u32,
    pub exclude: Vec<String>,
}

// Type aliases.
type YangListIterFn<P: Provider> =
    for<'a> fn(&'a P, &P::ListEntry<'a>) -> Option<ListIterator<'a, P>>;
type YangListNewFn<P: Provider> =
    for<'a> fn(&'a P, &P::ListEntry<'a>) -> Box<dyn YangObject + 'a>;
type YangContainerNewFn<P: Provider> =
    for<'a> fn(&'a P, &P::ListEntry<'a>) -> Option<Box<dyn YangObject + 'a>>;
type ListIterator<'a, P: Provider> =
    Box<dyn Iterator<Item = P::ListEntry<'a>> + 'a>;
type GetReceiver = oneshot::Receiver<Result<api::daemon::GetResponse, Error>>;

// ===== helper functions =====

fn iterate_node<'a, P>(
    provider: &'a P,
    dnode: &mut DataNodeRef<'_>,
    snode: &SchemaNode<'_>,
    list_entry: &P::ListEntry<'a>,
    relay_list: &mut Vec<GetReceiver>,
    first: bool,
    filter: &GetFilter,
    depth: u32,
) -> Result<(), Error>
where
    P: Provider,
{
    match snode.kind() {
        SchemaNodeKind::List => {
            iterate_list(
                provider, dnode, snode, list_entry, relay_list, filter, depth,
            )?;
        }
        SchemaNodeKind::Container => {
            iterate_container(
                provider, dnode, snode, list_entry, relay_list, first, filter,
                depth,
            )?;
        }
        SchemaNodeKind::Choice | SchemaNodeKind::Case => {
            iterate_children(
                provider, dnode, snode, list_entry, relay_list, filter, depth,
            )?;
        }
        _ => (),
    }

    Ok(())
}

fn iterate_list<'a, P>(
    provider: &'a P,
    dnode: &mut DataNodeRef<'_>,
    snode: &SchemaNode<'_>,
    parent_list_entry: &P::ListEntry<'a>,
    relay_list: &mut Vec<GetReceiver>,
    filter: &GetFilter,
    depth: u32,
) -> Result<(), Error>
where
    P: Provider,
{
    let module = snode.module();
    let snode_path = snode.data_path();
    if let Some(list_ops) = P::YANG_OPS.list.get(&snode_path)
        && let Some(list_iter) = (list_ops.iter)(provider, parent_list_entry)
    {
        for list_entry in list_iter {
            let obj = (list_ops.new)(provider, &list_entry);

            // Get list keys.
            let keys = obj.list_keys();

            // Add list entry node.
            let mut dnode =
                dnode.new_list(Some(&module), snode.name(), &keys).unwrap();

            // Initialize list entry.
            obj.into_data_node(&mut dnode);

            // Iterate over child nodes.
            iterate_children(
                provider,
                &mut dnode,
                snode,
                &list_entry,
                relay_list,
                filter,
                depth,
            )?;
        }
    }

    Ok(())
}

fn iterate_container<'a, P>(
    provider: &'a P,
    dnode: &mut DataNodeRef<'_>,
    snode: &SchemaNode<'_>,
    list_entry: &P::ListEntry<'a>,
    relay_list: &mut Vec<GetReceiver>,
    first: bool,
    filter: &GetFilter,
    depth: u32,
) -> Result<(), Error>
where
    P: Provider,
{
    let mut dnode = dnode.clone();
    let mut dnode_container;

    // Add container node.
    let dnode = if first {
        &mut dnode
    } else {
        let module = snode.module();
        dnode_container = dnode.new_inner(Some(&module), snode.name()).unwrap();
        &mut dnode_container
    };

    let snode_path = snode.data_path();
    if let Some(container_ops) = P::YANG_OPS.container.get(&snode_path)
        && let Some(obj) = (container_ops.new)(provider, list_entry)
    {
        // Initialize container node.
        obj.into_data_node(dnode);
    }

    iterate_children(provider, dnode, snode, list_entry, relay_list, filter, depth)?;

    // Remove the container node if it was added and remains empty.
    if !first && dnode.children().next().is_none() {
        dnode.remove();
    }

    Ok(())
}

fn iterate_children<'a, P>(
    provider: &'a P,
    dnode: &mut DataNodeRef<'_>,
    snode: &SchemaNode<'_>,
    list_entry: &P::ListEntry<'a>,
    relay_list: &mut Vec<GetReceiver>,
    filter: &GetFilter,
    depth: u32,
) -> Result<(), Error>
where
    P: Provider,
{
    // Depth check: stop recursion if max_depth reached.
    if filter.max_depth > 0 && depth >= filter.max_depth {
        return Ok(());
    }

    for snode in snode.children().filter(|snode| {
        matches!(
            snode.kind(),
            SchemaNodeKind::List
                | SchemaNodeKind::Container
                | SchemaNodeKind::Choice
                | SchemaNodeKind::Case
        )
    }) {
        // Skip exclude check for Choice/Case — they are transparent
        // schema wrappers; their real data children will be checked
        // individually on the next recursion.
        let module = snode.module();
        let node_name = snode.name();
        let module_name = module.name();
        if !matches!(
            snode.kind(),
            SchemaNodeKind::Choice | SchemaNodeKind::Case
        ) && filter.exclude.iter().any(|e| match e.split_once(':') {
            Some((m, n)) => m == module_name && n == node_name,
            None => e.as_str() == node_name,
        }) {
            continue;
        }

        // Check if the provider implements the child node.
        if let Some(child_nb_tx) = list_entry.child_task(module.name()) {
            // Prepare request to child task.
            let path =
                format!("{}/{}:{}", dnode.path(), module.name(), snode.name());
            let relay_rx = relay_request(child_nb_tx, path, filter, depth);
            relay_list.push(relay_rx);
            continue;
        }

        iterate_node(
            provider, dnode, &snode, list_entry, relay_list, false, filter,
            depth + 1,
        )?;
    }

    Ok(())
}

fn lookup_list_entry<'a, P>(
    provider: &'a P,
    dnode: &DataNodeRef<'_>,
) -> P::ListEntry<'a>
where
    P: Provider,
{
    let mut list_entry = Default::default();

    // Iterate over parent list entries starting from the root.
    for dnode in dnode
        .inclusive_ancestors()
        .filter(|dnode| dnode.schema().kind() == SchemaNodeKind::List)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        let snode_path = dnode.schema().data_path();
        let Some(list_ops) = P::YANG_OPS.list.get(&snode_path) else {
            continue;
        };

        // Obtain the list entry keys.
        let list_keys =
            dnode.list_keys().fold(String::new(), |mut list_keys, key| {
                let _ = write!(
                    list_keys,
                    "[{}='{}']",
                    key.schema().name(),
                    key.value_canonical().unwrap()
                );
                list_keys
            });

        // Find the list entry associated to the provided path.
        // If a matching entry isn't found (e.g. the neighbor session is
        // not Established and the RIB iterator filters it out), return
        // the default entry so callers produce empty output instead of
        // panicking on a mismatched variant.
        if let Some(entry) = {
            (list_ops.iter)(provider, &list_entry).and_then(|mut list_iter| {
                list_iter.find(|entry| {
                    let obj = (list_ops.new)(provider, entry);
                    list_keys == obj.list_keys()
                })
            })
        } {
            list_entry = entry;
        } else {
            return Default::default();
        }
    }

    list_entry
}

fn relay_request(
    nb_tx: NbDaemonSender,
    path: String,
    filter: &GetFilter,
    current_depth: u32,
) -> GetReceiver {
    let (responder_tx, responder_rx) = oneshot::channel();
    let request = api::daemon::GetRequest {
        path: Some(path),
        // Clamp to 1 so that saturating to 0 never flips the meaning
        // from "stop" to "unlimited" (0 = no limit in the protocol).
        max_depth: if filter.max_depth > 0 {
            filter.max_depth.saturating_sub(current_depth).max(1)
        } else {
            0
        },
        exclude: filter.exclude.clone(),
        responder: Some(responder_tx),
    };
    tokio::task::spawn(async move {
        nb_tx
            .send(api::daemon::Request::Get(request))
            .await
            .unwrap();
    });
    responder_rx
}

// ===== global functions =====

pub(crate) async fn process_stream_get<P>(
    provider: &P,
    path: String,
    max_depth: u32,
    exclude: Vec<String>,
    tx: Option<tokio::sync::mpsc::Sender<DataTree<'static>>>,
) where
    P: Provider,
{
    let Some(tx) = tx else { return };
    let yang_ctx = YANG_CTX.get().unwrap();

    // Split path into parent + terminal list node name.
    // StreamGet streams all entries, so the terminal must not carry
    // key predicates.
    let Some(last_slash) = path.rfind('/') else {
        return;
    };
    let parent_path = &path[..last_slash];
    let terminal = &path[last_slash + 1..];

    // Validate the parent path (which has predicates on interior
    // lists) and resolve the terminal via schema.
    let mut dtree_tmp = DataTree::new(yang_ctx);
    let Ok(Some(parent_dnode)) =
        dtree_tmp.new_path(parent_path, None, false)
    else {
        return;
    };
    let parent_snode = yang_ctx
        .find_path(&parent_dnode.schema().data_path())
        .unwrap();
    let Ok(snode) = parent_snode.find_path(terminal) else {
        return;
    };

    // Must be a list node.
    if snode.kind() != SchemaNodeKind::List {
        return;
    }

    let snode_path = snode.data_path();

    // Check if this provider owns this list AND if it's streamable.
    if let Some(list_ops) = P::YANG_OPS.list.get(&snode_path) {
        if !list_ops.streamable {
            return;
        }

        // Resolve parent list entry context from the parent data
        // node (not the terminal list — we iterate all its entries).
        let list_entry = lookup_list_entry(provider, &parent_dnode);

        // Build filter from client-provided parameters.
        let filter = GetFilter {
            max_depth,
            exclude,
        };

        // Iterate and stream entries.
        if let Some(list_iter) = (list_ops.iter)(provider, &list_entry) {
            for entry in list_iter {
                let obj = (list_ops.new)(provider, &entry);
                let keys = obj.list_keys();

                // Build mini DataTree for this single entry.
                let mut entry_dtree = DataTree::new(yang_ctx);
                let entry_path = format!("{}{}", path, keys);
                let Ok(Some(mut entry_dnode)) =
                    entry_dtree.new_path(&entry_path, None, false)
                else {
                    continue;
                };

                // Populate entry fields.
                obj.into_data_node(&mut entry_dnode);

                // Populate children (containers, nested lists) with filter.
                let mut relay_list = vec![];
                let _ = iterate_children(
                    provider,
                    &mut entry_dnode,
                    &snode,
                    &entry,
                    &mut relay_list,
                    &filter,
                    0,
                );

                // Collect child provider relay responses.
                for relay_rx in relay_list.drain(..) {
                    if let Ok(Ok(response)) = relay_rx.await {
                        let _ = entry_dtree.merge(&response.data);
                    }
                }

                // Send entry. Stop if receiver dropped (client
                // disconnected).
                if tx.send(entry_dtree).await.is_err() {
                    return;
                }
            }
        }
    } else {
        // List not in this provider — check if child provider owns it.
        // Walk path to find relay point and forward tx.
        let list_entry = lookup_list_entry(provider, &parent_dnode);
        let module = snode.module();
        if let Some(child_nb_tx) = list_entry.child_task(module.name()) {
            let request = api::daemon::StreamGetRequest {
                path,
                max_depth,
                exclude,
                tx: Some(tx),
            };
            let _ = child_nb_tx
                .send(api::daemon::Request::StreamGet(request))
                .await;
        }
    }
}

pub(crate) fn process_get<P>(
    provider: &P,
    path: Option<String>,
    max_depth: u32,
    exclude: Vec<String>,
) -> Result<api::daemon::GetResponse, Error>
where
    P: Provider,
{
    let yang_ctx = YANG_CTX.get().unwrap();

    let mut dtree = DataTree::new(yang_ctx);

    // Populate data tree with path requested by the user.
    let mut relay_list = vec![];
    let path = path.unwrap_or(provider.top_level_node());
    let mut dnode = dtree
        .new_path(&path, None, false)
        .map_err(Error::YangInvalidPath)?
        .unwrap();
    let list_entry = lookup_list_entry(provider, &dnode);
    let snode = yang_ctx.find_path(&dnode.schema().data_path()).unwrap();

    // Build filter options.
    let filter = GetFilter {
        max_depth,
        exclude,
    };

    // Check if the provider implements the child node.
    let module = snode.module();
    if let Some(child_nb_tx) = list_entry.child_task(module.name()) {
        // Prepare request to child task.
        let relay_rx = relay_request(child_nb_tx, path, &filter, 0);
        relay_list.push(relay_rx);
    } else {
        // If a list entry was given, iterate over that list entry.
        if snode.kind() == SchemaNodeKind::List {
            iterate_children(
                provider,
                &mut dnode,
                &snode,
                &list_entry,
                &mut relay_list,
                &filter,
                0,
            )?;
        } else {
            iterate_node(
                provider,
                &mut dnode,
                &snode,
                &list_entry,
                &mut relay_list,
                true,
                &filter,
                0,
            )?;
        }
    }

    // Collect responses from all relayed requests.
    for relay_rx in relay_list {
        let response = relay_rx.blocking_recv().unwrap()?;
        dtree
            .merge(&response.data)
            .map_err(Error::YangInvalidData)?;
    }

    Ok(api::daemon::GetResponse { data: dtree })
}
