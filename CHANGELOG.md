# Changelog

## [0.14.2](https://github.com/gardnmi/boomux/compare/v0.14.1...v0.14.2) (2026-08-14)


### Bug Fixes

* **tui:** keep navigation responsive during refresh ([#157](https://github.com/gardnmi/boomux/issues/157)) ([d22460a](https://github.com/gardnmi/boomux/commit/d22460adf78a0f1891cd9cbb11959f574233cae4))

## [0.14.1](https://github.com/gardnmi/boomux/compare/v0.14.0...v0.14.1) (2026-08-14)


### Bug Fixes

* **tui:** guide first-time workspace creation ([#144](https://github.com/gardnmi/boomux/issues/144)) ([28c2b61](https://github.com/gardnmi/boomux/commit/28c2b61c910cec59480653886010f794f6d4279f))

## [0.14.0](https://github.com/gardnmi/boomux/compare/v0.13.0...v0.14.0) (2026-08-14)


### Features

* invoke individual launchers ([#135](https://github.com/gardnmi/boomux/issues/135)) ([ee74631](https://github.com/gardnmi/boomux/commit/ee746314d68075ca021b2d45716f267f99556f8e))


### Performance Improvements

* **daemon:** bound connection and response resources ([#138](https://github.com/gardnmi/boomux/issues/138)) ([554fb1c](https://github.com/gardnmi/boomux/commit/554fb1c34f4c1a2af20500bcad8921bb810009bf))
* **daemon:** move persistence out of PTY-critical coordination ([#133](https://github.com/gardnmi/boomux/issues/133)) ([cee1caa](https://github.com/gardnmi/boomux/commit/cee1caa6998f11aeee8c53af1a80dca14d83b657))
* **daemon:** remove global serialization from PTY output ([#136](https://github.com/gardnmi/boomux/issues/136)) ([e25d8d2](https://github.com/gardnmi/boomux/commit/e25d8d2df3621cd38d6f4bb62d83c294003bea3c))
* **dashboard:** refresh from events instead of full polling ([#131](https://github.com/gardnmi/boomux/issues/131)) ([cb5d6a4](https://github.com/gardnmi/boomux/commit/cb5d6a45380be70b2af22796af4bf3f243eb2a42))
* **terminal:** bound preview work before formatting ([#137](https://github.com/gardnmi/boomux/issues/137)) ([8bfc0fb](https://github.com/gardnmi/boomux/commit/8bfc0fbef70f9d1fefbc16f9570b4340742c5ac1))


### Code Refactoring

* **cli:** centralize command metadata ([#126](https://github.com/gardnmi/boomux/issues/126)) ([264df3d](https://github.com/gardnmi/boomux/commit/264df3dfa36d52989374261d77b9f0010a40df56))
* **daemon:** replace whole-registry rollback with lifecycle transactions ([#141](https://github.com/gardnmi/boomux/issues/141)) ([d3c0ac8](https://github.com/gardnmi/boomux/commit/d3c0ac88ea9aa48038b811a282470c55659b7c10))
* **daemon:** separate durable state from runtime services ([#139](https://github.com/gardnmi/boomux/issues/139)) ([434ba90](https://github.com/gardnmi/boomux/commit/434ba906b9a5ea7489197da93d8d6baac6fae42f))
* **dashboard:** add typed view projection ([#127](https://github.com/gardnmi/boomux/issues/127)) ([bf130da](https://github.com/gardnmi/boomux/commit/bf130da115340db376f4c1182f9127e017a0ad05))
* **errors:** preserve typed daemon and client failures ([316b4af](https://github.com/gardnmi/boomux/commit/316b4af7a73d1b26138430ef6ca6e37ef5a63579)), closes [#123](https://github.com/gardnmi/boomux/issues/123)
* **integrations:** centralize capability descriptors ([88e1321](https://github.com/gardnmi/boomux/commit/88e1321d45fc9ea9a6652dce263c5a8543b35a46)), closes [#122](https://github.com/gardnmi/boomux/issues/122)
* **protocol:** centralize feature compatibility policy ([#129](https://github.com/gardnmi/boomux/issues/129)) ([57dc608](https://github.com/gardnmi/boomux/commit/57dc60845c60c4e4eb0ed6736c810ec852797bfb))
* **tui:** separate model updates from effects ([#130](https://github.com/gardnmi/boomux/issues/130)) ([2c83a43](https://github.com/gardnmi/boomux/commit/2c83a431431ffe33bc3f9dc6a3126cab5ace36b6))

## [0.13.0](https://github.com/gardnmi/boomux/compare/v0.12.0...v0.13.0) (2026-08-12)


### Features

* **doctor:** report version and platform ([#108](https://github.com/gardnmi/boomux/issues/108)) ([d7dbf6f](https://github.com/gardnmi/boomux/commit/d7dbf6fe687cba55ec7a634db1f60591d18fa061))

## [0.12.0](https://github.com/gardnmi/boomux/compare/v0.11.0...v0.12.0) (2026-08-12)


### Features

* **tui:** improve workspace items table ([#102](https://github.com/gardnmi/boomux/issues/102)) ([3ebe01c](https://github.com/gardnmi/boomux/commit/3ebe01c0ddf6763afcb06762fdc6a585fdb28172))
* **tui:** organize agent session preview ([#104](https://github.com/gardnmi/boomux/issues/104)) ([25e885d](https://github.com/gardnmi/boomux/commit/25e885dcc04bb6608bdae2e769233eb7a4707808))
* **tui:** organize shell preview ([#105](https://github.com/gardnmi/boomux/issues/105)) ([11758f2](https://github.com/gardnmi/boomux/commit/11758f2f2e79ee8e1c0bf91945301fcdf103c54b))

## [0.11.0](https://github.com/gardnmi/boomux/compare/v0.10.1...v0.11.0) (2026-08-12)


### Features

* **tui:** improve shell management table ([#100](https://github.com/gardnmi/boomux/issues/100)) ([b9f8f00](https://github.com/gardnmi/boomux/commit/b9f8f00a38d03835a7c5432f0ca7fb5f8afc5e85))

## [0.10.1](https://github.com/gardnmi/boomux/compare/v0.10.0...v0.10.1) (2026-08-12)


### Bug Fixes

* **session:** capture complete OpenCode exports ([#98](https://github.com/gardnmi/boomux/issues/98)) ([0559943](https://github.com/gardnmi/boomux/commit/0559943940383e4d11087bfa77247709174999bd))

## [0.10.0](https://github.com/gardnmi/boomux/compare/v0.9.1...v0.10.0) (2026-08-11)


### Features

* **tui:** improve agent management table ([#96](https://github.com/gardnmi/boomux/issues/96)) ([f122411](https://github.com/gardnmi/boomux/commit/f122411fca06d67100d70b264d60138ffd4f85e9))

## [0.9.1](https://github.com/gardnmi/boomux/compare/v0.9.0...v0.9.1) (2026-08-11)


### Bug Fixes

* **tui:** keep focus following within active tab ([#94](https://github.com/gardnmi/boomux/issues/94)) ([9692622](https://github.com/gardnmi/boomux/commit/969262212da215b2847e906bdab230ab58883a44))

## [0.9.0](https://github.com/gardnmi/boomux/compare/v0.8.0...v0.9.0) (2026-08-11)


### Features

* **tui:** focus dashboard tabs on agents and shells ([#92](https://github.com/gardnmi/boomux/issues/92)) ([a3df999](https://github.com/gardnmi/boomux/commit/a3df99907d481db317c97e5e2793c89ee10d0295))

## [0.8.0](https://github.com/gardnmi/boomux/compare/v0.7.0...v0.8.0) (2026-08-11)


### Features

* recover sessions after cold restarts ([#88](https://github.com/gardnmi/boomux/issues/88)) ([5779b9e](https://github.com/gardnmi/boomux/commit/5779b9e7450d2ce0d896e7275388fad6172bf952))


### Bug Fixes

* remove stale overview attention and harden process timing ([#91](https://github.com/gardnmi/boomux/issues/91)) ([6caaf8a](https://github.com/gardnmi/boomux/commit/6caaf8ae58d398203efff4f60ea41effad6bda34))
* **tui:** expose by-name workspace creation ([#89](https://github.com/gardnmi/boomux/issues/89)) ([5cf0e0d](https://github.com/gardnmi/boomux/commit/5cf0e0d8ac0bdf9e4d8ca3fa25db2b4c3cda9da8))

## [0.7.0](https://github.com/gardnmi/boomux/compare/v0.6.0...v0.7.0) (2026-08-10)


### Features

* **tui:** pin dashboard selection ([#86](https://github.com/gardnmi/boomux/issues/86)) ([390fb78](https://github.com/gardnmi/boomux/commit/390fb78d4c311c296d5bdae0bf3316d19891ed4a))

## [0.6.0](https://github.com/gardnmi/boomux/compare/v0.5.1...v0.6.0) (2026-08-10)


### Features

* **tui:** render colored shell previews ([#85](https://github.com/gardnmi/boomux/issues/85)) ([61d7dc2](https://github.com/gardnmi/boomux/commit/61d7dc21b4732386b33164e741c864d4ee1c777b))


### Bug Fixes

* **terminal:** preserve reattached output rendering ([#83](https://github.com/gardnmi/boomux/issues/83)) ([e27d535](https://github.com/gardnmi/boomux/commit/e27d535295dbd8c4f9562eda70fc2ffa69c928e5))

## [0.5.1](https://github.com/gardnmi/boomux/compare/v0.5.0...v0.5.1) (2026-08-10)


### Bug Fixes

* **workspace:** preserve project cwd for new shells ([#81](https://github.com/gardnmi/boomux/issues/81)) ([cdcfaa4](https://github.com/gardnmi/boomux/commit/cdcfaa44fffc09b2ae167c94f7b79ae78af26169))

## [0.5.0](https://github.com/gardnmi/boomux/compare/v0.4.2...v0.5.0) (2026-08-10)


### Features

* **tui:** follow focused terminals ([#79](https://github.com/gardnmi/boomux/issues/79)) ([d91a070](https://github.com/gardnmi/boomux/commit/d91a070d99140827cb17fa0d404039f01d9b03d6))

## [0.4.2](https://github.com/gardnmi/boomux/compare/v0.4.1...v0.4.2) (2026-08-10)


### Bug Fixes

* **notifications:** deliver agent completion alerts ([#75](https://github.com/gardnmi/boomux/issues/75)) ([bfa6ee3](https://github.com/gardnmi/boomux/commit/bfa6ee34dbe969be3e6e74cb96dea9792209b73d))


### Performance Improvements

* **opencode:** coalesce working activity reports ([#77](https://github.com/gardnmi/boomux/issues/77)) ([e6aa0fa](https://github.com/gardnmi/boomux/commit/e6aa0fa953c38ef997e7faf18880869ccb2c5d03))

## [0.4.1](https://github.com/gardnmi/boomux/compare/v0.4.0...v0.4.1) (2026-08-10)


### Performance Improvements

* **terminal:** avoid cloning screen per output chunk ([#73](https://github.com/gardnmi/boomux/issues/73)) ([87614e2](https://github.com/gardnmi/boomux/commit/87614e25f7b5ae658c61fbd5c2db015bca082f78))

## [0.4.0](https://github.com/gardnmi/boomux/compare/v0.3.0...v0.4.0) (2026-08-10)


### Features

* **notifications:** add sound delivery ([#68](https://github.com/gardnmi/boomux/issues/68)) ([26ebf37](https://github.com/gardnmi/boomux/commit/26ebf37f2e4741fd4712e7f68286f951134d6036))

## [0.3.0](https://github.com/gardnmi/boomux/compare/v0.2.0...v0.3.0) (2026-08-10)


### Features

* **tui:** add grouped command palette ([#65](https://github.com/gardnmi/boomux/issues/65)) ([51e0d0b](https://github.com/gardnmi/boomux/commit/51e0d0b9f27ad122409ce0558b5c2d174debbc70))


### Bug Fixes

* **tui:** simplify dashboard tables ([#67](https://github.com/gardnmi/boomux/issues/67)) ([1ff45e8](https://github.com/gardnmi/boomux/commit/1ff45e8937a92fe637373c6d8df2ed6165a52d03))

## [0.2.0](https://github.com/gardnmi/boomux/compare/v0.1.0...v0.2.0) (2026-08-09)


### Features

* add integration management ([#56](https://github.com/gardnmi/boomux/issues/56)) ([101f4e0](https://github.com/gardnmi/boomux/commit/101f4e09f51407b45be1c205d63bac7049b6ef10))
* guide integration setup ([#61](https://github.com/gardnmi/boomux/issues/61)) ([1710dfd](https://github.com/gardnmi/boomux/commit/1710dfdfc64f52031f60f95abc84029279a9f261))
* improve shell identity and dashboard previews ([1646dc7](https://github.com/gardnmi/boomux/commit/1646dc741b9025c3a4d4c346a10280d1178cbbe6))
* preview integration installs ([#59](https://github.com/gardnmi/boomux/issues/59)) ([3d8cce6](https://github.com/gardnmi/boomux/commit/3d8cce69fae83bd60c6dce7960d4c4be3cfe0b91))
* safely uninstall integrations ([#60](https://github.com/gardnmi/boomux/issues/60)) ([2625539](https://github.com/gardnmi/boomux/commit/26255396e36f48bb9a51f19e81e7368a4741200b))
* **tui:** add scrollable shell previews ([#54](https://github.com/gardnmi/boomux/issues/54)) ([ba478f4](https://github.com/gardnmi/boomux/commit/ba478f4aad469fed99c974a84aa5f55a8c954d5b))
* verify integration reporting ([#58](https://github.com/gardnmi/boomux/issues/58)) ([0a74dde](https://github.com/gardnmi/boomux/commit/0a74dde831d36210750e095330dea0f9b7801a5d))
