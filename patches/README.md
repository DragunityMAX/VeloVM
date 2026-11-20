# Vendored OpenVMM Patches

`openvmm/` vendors a fork of [`microsoft/openvmm`](https://github.com/microsoft/openvmm) starting from upstream commit `c4419be15ab702194f0a25c294bd2f6187804824`. Changes made inside vendored upstream crates are tracked here as named patches so they can be audited, preserved, or dropped cleanly during upstream syncs.

## Patch Inventory

### `0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch`
- **Target:** `openvmm/support/mesh/mesh_rpc/src/server.rs`
- **Summary:** Cancels the connection `CancelContext` and skips writing responses in `mesh_rpc::Server::serve_connection` when the ttrpc client socket closes or hits EOF.
- **Behavioral impact:** Affects every ttrpc service hosted by `mesh_rpc::Server`—any client that closes its socket before receiving a reply cancels its pending RPCs on that connection and drops their responses.
- **Why carried:** Required for `openvmmsd` daemon restarts without stopping VMs (`WaitVM` holds a single per-VM waiter slot in `VmService`, so closing `openvmmsd`'s socket must release `WaitVM` for the next daemon to re-adopt the VM cleanly).

## Verifying and Re-applying

From the repository root:

```bash
# Confirm the patch matches the current vendored tree:
git apply -R --check patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch

# Apply to a clean upstream openvmm checkout (stripping the leading `openvmm/` path segment):
cd openvmm && git apply -p2 ../patches/0001-mesh-rpc-cancel-pending-rpcs-on-client-disconnect.patch
```
