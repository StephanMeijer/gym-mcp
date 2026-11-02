# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/StephanMeijer/gym-mcp/compare/v0.2.0...v0.3.0) - 2026-11-02

### Added

- *(settings)* goals notes, priority muscle groups and limitations
- *(search)* muscle_group_volume for any period
- *(overview)* per-muscle frequency, volume and effort
- *(sets)* record the machine brand per set and compare history per machine
- *(exercises)* [**breaking**] split muscle groups into primary and secondary with a finer list
- *(sets)* log cardio with distance, intensity and an exercise kind

## [0.2.0](https://github.com/StephanMeijer/gym-mcp/compare/v0.1.4...v0.2.0) - 2026-11-02

### Added

- *(sets)* record rpe, rest and a training goal, and warn on off-rack weights

### Other

- [**breaking**] replace the equipment entity with free-text equipment on gyms and exercises

## [0.1.4](https://github.com/StephanMeijer/gym-mcp/compare/v0.1.3...v0.1.4) - 2026-11-02

### Added

- *(mcp)* annotate tools and run read tools in read-only transactions

## [0.1.3](https://github.com/StephanMeijer/gym-mcp/compare/v0.1.2...v0.1.3) - 2026-11-02

### Added

- *(equipment)* add descriptions and an equipment_update tool

## [0.1.2](https://github.com/StephanMeijer/gym-mcp/compare/v0.1.1...v0.1.2) - 2026-11-02

### Fixed

- *(ci)* let docker.yml inherit caller permissions so bot and fork PRs run

## [0.1.1](https://github.com/StephanMeijer/gym-mcp/compare/v0.1.0...v0.1.1) - 2026-11-02

### Fixed

- *(db)* enable rustls so sqlx can connect with sslmode require/verify-full

### Other

- strip symbols from release binary
