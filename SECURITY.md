# Security

Bcode is early-alpha software that can read repositories, execute tools, and handle provider credentials. Permission checks are application policy, not an operating-system sandbox. Native plugins execute code in the host process and must be trusted. Review configuration and plugin provenance before use.

Do not publish credentials, private source code, complete session archives, or sensitive tool output in bug reports. Reproduce issues using synthetic data where possible.

Report suspected vulnerabilities privately to [bradensteffaniak@gmail.com](mailto:bradensteffaniak@gmail.com). Include the commit/version, platform, relevant feature configuration, expected boundary, and a minimal reproduction. Ordinary non-sensitive bugs belong in [GitHub Issues](https://github.com/BSteffaniak/bcode/issues).

There is no promised support window or response-time SLA. A source build passing tests is not an independent security audit.
