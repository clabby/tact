# Memory server example

This example runs the `tact-memory` HTTP server with a bounded in-memory store. It supports
multiple authenticated writer and reader namespaces and is intended for local testing of remote
Tact memory. All records exist only for the lifetime of the server process and are discarded when
it stops. This is not a production deployment.

From the repository root, set one environment variable for each credential and start the server:

```console
export ALICE_MEMORY_TOKEN='alice-local-test-token-000000000001'
export BOB_MEMORY_TOKEN='bob-local-test-token-00000000000002'
export OBSERVER_MEMORY_TOKEN='observer-local-test-token-00000001'
cargo run -p tact-memory-server-example -- \
  --listen 127.0.0.1:8787 \
  --writer alice=ALICE_MEMORY_TOKEN \
  --writer bob=BOB_MEMORY_TOKEN \
  --reader observer=OBSERVER_MEMORY_TOKEN
```

`--writer NAMESPACE=ENV_VAR` grants write access to a namespace. `--reader NAMESPACE=ENV_VAR`
grants read-only access. The server reads token values from the named environment variables and
prints its bound address. Stop it with Ctrl-C. The server handles Ctrl-C and termination signals
gracefully, but its process-lifetime records are not saved during shutdown.

Configure a Tact client with the matching endpoint, namespace, and token:

```toml
[memory]
enabled = true

[memory.remote]
endpoint = "http://127.0.0.1:8787/"
namespace = "alice"
bearer_token = "alice-local-test-token-000000000001"
workspace_roots = ["/absolute/path/to/this/repository"]
```

Inside the configured workspace, Tact uses the remote server. Use `tact memory upload` to send a
local snapshot to the writer namespace and `tact memory pull --namespace alice` to merge selected
remote records into local memory before the server stops.
