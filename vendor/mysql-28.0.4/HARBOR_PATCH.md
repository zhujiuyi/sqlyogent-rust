# Local MySQL driver patch

This directory contains the library sources, package metadata and licenses from `mysql` 28.0.4. The source archive matched the registry checksum recorded in Cargo.lock before the path patch was introduced.

`ca3de8a73b7c70f76dbcdd4b6abc9b53bbb6c4f7bd6c79d8e8a7ea2157c02487`

The patch gives `SslOpts` an optional TLS server identity independent of the TCP destination. Harbor SQL connects to an SSH forward on the loopback interface while verifying the database certificate against the remote database host. Unset options retain the upstream behaviour.
