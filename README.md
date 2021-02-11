# Actix-Web Fork Information
This is a fork of actix-web that is kept up-to-date with their master
branch but contains our own patches and preferences.

The following has been modified:

- content_type from named file can be obtained directly
- added time-to-first-byte logging to Logger middeware
- error response logging is moved to logger middleware and logger will
  log all errors returned rather than only internal server errors
  
## License

This project is licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))

at your option.

## Code of Conduct

Contribution to the actix-web crate is organized under the terms of the Contributor Covenant, the
maintainers of Actix web, promises to intervene to uphold that code of conduct.
