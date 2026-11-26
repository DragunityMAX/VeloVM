// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Error;
use crate::L2SocketEndpoint;
use net_backend::resolve::ResolveEndpointParams;
use net_backend::resolve::ResolvedEndpoint;
use net_backend_resources::l2::L2SocketHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;

pub struct L2SocketResolver;

declare_static_resolver! {
    L2SocketResolver,
    (NetEndpointHandleKind, L2SocketHandle),
}

impl ResolveResource<NetEndpointHandleKind, L2SocketHandle> for L2SocketResolver {
    type Output = ResolvedEndpoint;
    type Error = Error;

    fn resolve(
        &self,
        resource: L2SocketHandle,
        _input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        Ok(L2SocketEndpoint::connect(resource.path)?.into())
    }
}
