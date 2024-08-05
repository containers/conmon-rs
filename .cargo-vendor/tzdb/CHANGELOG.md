## Changes between the versions

### 0.6.1 (2023-12-30)

* Split into `tzdb` and `tzdb_data`
* Optimize lookup. It's ~39% faster now.

### 0.6.0 (2023-12-29)

* Unchanged stable release

### 0.6.0-pre.1 (2023-12-27)

* Make `iana_time_zone` inclusion optional

### 0.5.9 (2023-12-27)

* Update to Time Zone Database release [2023d](https://mm.icann.org/pipermail/tz-announce/2023-December/000080.html)

### ~~0.5.8 (2023-12-27)~~

### 0.5.7 (2023-05-11)

* Fewer macros = faster compile times
* Update to Time Zone Database release [2023c](https://mm.icann.org/pipermail/tz-announce/2023-March/000079.html)

### 0.5.6 (2023-03-24)

* Update to Time Zone Database release [2023b](https://mm.icann.org/pipermail/tz-announce/2023-March/000078.html)

### 0.5.5 (2023-03-24)

* Update to Time Zone Database release [2023a](https://mm.icann.org/pipermail/tz-announce/2023-March/000077.html)
* Remove "etc/localtime" as it should not be part of this library

### ~~0.5.4 (2023-03-24)~~

### 0.5.3 (2023-01-01)

* No need to use `unsafe` functions

### 0.5.2 (2022-12-22)

* Prepare v0.5.x branch so that v0.3.x can re-export it

### 0.5.1 (2022-11-30)

* Update to Time Zone Database release [2022g](https://mm.icann.org/pipermail/tz-announce/2022-November/000076.html)

### 0.5.0 (2022-11-24)

* Release v0.5.0

#### 0.5.0-pre.4 (2022-10-29)

* Update to Time Zone Database release [2022f](https://mm.icann.org/pipermail/tz-announce/2022-October/000075.html)
* Use `edition = "2021"`

#### 0.5.0-pre.3 (2022-10-12)

* Remove `utcnow` integration
* Simplify by removing `no_std` support
* Update to Time Zone Database release [2022e](https://mm.icann.org/pipermail/tz-announce/2022-October/000074.html)

#### 0.5.0-pre.2 (2022-09-25)

* Update to Time Zone Database release [2022d](https://mm.icann.org/pipermail/tz-announce/2022-September/000073.html)
* Update to iana-time-zone 0.1.50

#### 0.5.0-pre.1 (2022-09-14)

* Simplify a lot by removing feature gates [[#123](https://github.com/Kijewski/tzdb/pull/123)]

### 0.4.5 (2022-08-31)

* Remove [phf](https://crates.io/crates/phf) dependency

### 0.4.4 (2022-08-16)

* Update to [Time Zone Database 2022c](https://mm.icann.org/pipermail/tz-announce/2022-August/000072.html)

### 0.4.3 (2022-08-12)

* Update [iana-time-zone](https://crates.io/crates/iana-time-zone) to fix more issues on CentOS 7
  ([#49](https://github.com/strawlab/iana-time-zone/pull/49)), and not to depend on core-foundation
  ([#50](https://github.com/strawlab/iana-time-zone/pull/50))

### 0.4.2 (2022-08-11)

* Update to [Time Zone Database 2022b](https://mm.icann.org/pipermail/tz-announce/2022-August/000071.html)

### 0.4.1 (2022-08-10)

* Update [iana-time-zone](https://crates.io/crates/iana-time-zone) to v0.1.42
  in order to fix problems on CentOS
  ([#48](https://github.com/strawlab/iana-time-zone/pull/48))

### 0.4.0 (2022-08-05)

* Increase msrv to 1.60
* Add `now` module, which uses [`utcnow()`](https://crates.io/crates/utcnow),
  and works in `#[no_std]`

### 0.3.4 (2022-08-02)

* Fix endianness issues for PowerPCs

### 0.3.3 (2022-08-01)

* Update [tz-rs](https://crates.io/crates/tz-rs) to v0.6.12 to work in a no-std context
  ([#33](https://github.com/x-hgg-x/tz-rs/pull/33))
* Expand documentation
* Add features `std`, `alloc`, and `fallback` (unused until the next breaking change)

### 0.3.2 (2022-07-30)

* Update [iana-time-zone](https://crates.io/crates/iana-time-zone) to implement
  [`local_tz()`](https://docs.rs/tzdb/0.3.2/tzdb/fn.local_tz.html) for
  Illumos ([#44](https://github.com/strawlab/iana-time-zone/pull/44)) and
  Android ([#45](https://github.com/strawlab/iana-time-zone/pull/45))

### 0.3.1 (2022-07-23)

* Update [iana-time-zone](https://crates.io/crates/iana-time-zone) to implement
  [`local_tz()`](https://docs.rs/tzdb/0.2.6/tzdb/fn.local_tz.html) for
  iOS ([#41](https://github.com/strawlab/iana-time-zone/pull/41))

### 0.3.0 (2022-07-21)

* Remove serde-as feature. The feature is very unrelated to goals of the crate, so it should be
  moved somewhere else
* Split up `generated.rs` to speed up compilation if not all features are selected
* Reduce msrv to 1.55

### 0.2.7 (2022-06-30)

* Fix error if build and target platform have different pointer widths

### 0.2.6 (2022-06-29)

* Update [iana-time-zone](https://crates.io/crates/iana-time-zone) to implement
  [`local_tz()`](https://docs.rs/tzdb/0.2.6/tzdb/fn.local_tz.html) for
  Wasm ([#38](https://github.com/strawlab/iana-time-zone/pull/38)), and
  {Free,Net,Open,Dragonfly}BSD ([#39](https://github.com/strawlab/iana-time-zone/pull/39))

### 0.2.5 (2022-06-26)

* Ensure `-Zminimal-versions` works

### 0.2.4 (2022-06-08)

* Fix missing import if the project is used with `default-features = false`

### 0.2.3 (2022-04-15)

* Fix lookup error for names containing underscores

### 0.2.2 (2022-03-27)

* Bump dependency versions

### 0.2.1 (2022-03-27)

* Fix typos
* Introduce `VERSION` and `VERSION_HASH`

### 0.2.0 (2022-03-17)

* Update to 2022a
* Make the unparsed binary time zone data available
* Simplify the library by removing the trait TimeZoneExt:

   * `TimeZoneExt::from_db()` is now `tz_by_name()`
   * `TimeZoneExt::local_from_db()` is now `local_tz()`
   * `TimeZoneExt::names_in_db()` is now `TZ_NAMES`

### 0.1.4 (2022-03-17)

* Re-export v0.2 with old names and default features

### 0.1.3 (2022-03-03)

* Optimize `DateTime` deserialization to work without dynamic allocation
  ([tz-rs#22](https://github.com/x-hgg-x/tz-rs/pull/22))

### 0.1.2 (2022-03-02)

* Include “backzone” data to include pre-1970 information for some more time zones

### 0.1.1 (2022-03-01)

* Make `UtcDateTime`/`DateTime` serializable with `serde` using `serde_with`

### 0.1.0 (2022-02-28)

* Initial release
