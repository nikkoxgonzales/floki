# floki-proto

floki-proto owns the serde request/response types for the newline-delimited JSON named-pipe protocol shared by the service, CLI, and UI; it contains no logic.

Wire format: each frame is one compact JSON object plus `'\n'`, sent over the
named pipe `\\.\pipe\floki` (override with env `FLOKI_PIPE`). Every object —
requests, responses, and the nested `IndexState` — carries a `"type"` tag with
a `snake_case` variant name; all fields are `snake_case`. One request gets
exactly one response; the connection stays open. Frames larger than 64 MiB are
rejected. `Sort` is `name_asc` / `name_desc` / `path_asc` / `path_desc` /
`modified_asc` / `modified_desc` / `created_asc` / `created_desc`.
`Hello` (protocol handshake, `PROTOCOL_VERSION = 3`) is an approved addition
on top of SPEC section 6. v2 adds per-volume `enabled` / `monitor` flags on
`status` volumes plus the NTFS-targets ops: `volumes_set_enabled`,
`volumes_set_monitor`, `volumes_remove`, `targets_config_get` /
`targets_config_set` (the latter answered by `targets_config`). v3 adds
`size` / `modified_ms` / `created_ms` to `HitRow` (statted service-side per
returned row; absent fields parse as `null`) and the four time-based sorts.
Requests (one line each):

```json
{"type":"hello"}
{"type":"search","query":"*.rs","max_results":100,"offset":0,"sort":"name_asc","client_id":7}
{"type":"status"}
{"type":"rescan","volume":"C"}
{"type":"volumes_set_enabled","volume":"C","enabled":false}
{"type":"volumes_set_monitor","volume":"C","monitor":true}
{"type":"volumes_remove","volume":"D"}
{"type":"targets_config_get"}
{"type":"targets_config_set","auto_include_fixed":true,"auto_include_removable":false,"auto_remove_offline":true}
{"type":"shutdown"}
```

{"type":"hello","protocol":3,"service_version":"0.1.0"}
{"type":"results","total":2,"hits":[{"name":"foo.txt","path":"C:\\docs","is_dir":false,"size":12,"modified_ms":1700000000000,"created_ms":1699000000000}],"elapsed_us":42}
{"type":"status","entries":3,"volumes":[{"letter":"C","entries":3,"next_usn":1234,"live":true,"enabled":true,"monitor":true}],"rss_bytes":1048576,"uptime_s":9,"state":{"type":"ready"}}
{"type":"targets_config","auto_include_fixed":true,"auto_include_removable":false,"auto_remove_offline":true}
{"type":"ok"}
{"type":"error","message":"boom"}
```

(`state` is `{"type":"loading"}`, `{"type":"scanning","volume":"D","done":512}`,
or `{"type":"ready"}`.)
