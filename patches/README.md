# Vendored OpenVMM Patches

`openvmm/` vendors a fork of [`microsoft/openvmm`](https://github.com/microsoft/openvmm) starting from upstream commit `c4419be15ab702194f0a25c294bd2f6187804824`. Changes made inside vendored upstream crates are tracked here as named patches so they can be audited, preserved, or dropped cleanly during upstream syncs.

## Patch Inventory

### `0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch`
- **Target:** `openvmm/support/mesh/mesh_rpc/src/server.rs`
- **Summary:** Cancels the connection `CancelContext` and skips writing responses in `mesh_rpc::Server::serve_connection` when the ttrpc client socket disconnects. On Unix, read EOF alone (such as a client half-close via `shutdown(SHUT_WR)`) keeps the connection open until `send_task` finishes draining in-flight replies, cancelling only if `POLLHUP`/`POLLERR` or a read/write error occurs. (On Windows `AF_UNIX`, `closesocket` cannot be distinguished from `SHUT_WR` at read EOF without writing, so read EOF still cancels.)
- **Behavioral impact:** Affects every ttrpc service hosted by `mesh_rpc::Server`—any client that disconnects before receiving a reply cancels its pending RPCs on that connection via `CancelContext` and drops their responses. In particular, if `openvmmsd` restarts or disconnects while a `SaveVm`, `RestoreVm`, or `ShutdownVm` RPC is running, that operation is now cancelled instead of finishing.
- **Why carried:** Required for `openvmmsd` daemon restarts without stopping VMs (`WaitVM` holds a single per-VM waiter slot in `VmService`, so closing `openvmmsd`'s socket must release `WaitVM` for the next daemon to re-adopt the VM cleanly).

## Verifying and Re-applying

From the repository root:

```bash
# Confirm the patch matches the current vendored tree:
git apply -R --check patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch

# Apply to a clean upstream openvmm checkout (stripping the leading `openvmm/` path segment):
cd openvmm && git apply -p2 ../patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch
```
