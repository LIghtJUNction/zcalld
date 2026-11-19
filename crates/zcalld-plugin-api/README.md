# zcalld-plugin-api

The allocator-free, versioned native interface between zcalld and explicitly
loaded gateway plugins. It uses zplugin's checked C ABI and caller-owned byte
buffers. Native plugins are trusted executable code.

Implement `GatewayPlugin`, export `GatewayPluginExport<T>` with zplugin, and build
a `cdylib`. Methods configure one instance, select a route, transform a request,
and observe completed calls. The host creates an independent instance per worker;
plugin objects never cross worker threads.

See [the plugin guide](https://github.com/LIghtJUNction/zcalld/blob/main/docs/PLUGINS.md)
for the complete contract and working aliases/policy plugins.
