# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/StephanMeijer/gym-mcp/compare/v0.4.0...v0.4.1) - 2026-11-03

### Fixed

- *(tools)* mark body_note_update destructive
- *(auth)* refresh keys on a typed unknown-kid error, trust discovered iss
- *(search)* rank note matches before applying the limit

### Other

- Merge pull request #56 from StephanMeijer/fix/search-rank-limit
- *(measurement)* match schema units exhaustively per kind
- *(measurement)* derive the stored unit from the accepted units
- pin every action to a commit in actions.lock
- fail on dependency advisories with cargo-deny
- *(readme)* list measurements, body notes and all 33 tools
- *(overview)* find each exercise's last session with a window function

## [0.4.0](https://github.com/StephanMeijer/gym-mcp/compare/v0.3.1...v0.4.0) - 2026-11-03

### Added

- *(search)* [**breaking**] return muscle volume and max reps as rows
- *(body-notes)* log dated aches with location and severity
- *(workouts)* mark workout and set times as estimated
- *(workouts)* move a workout together with its sets
- *(sets)* record heart rate, calories, incline and speed on cardio sets
- *(sets)* correct or delete several sets in one call
- *(sets)* record grip or variant per set and compare history per variant
- *(sets)* count unilateral sets once per side and record load basis
- *(gyms)* append to a gym's equipment and notes text
- *(sets)* record set type and count only working sets

### Fixed

- *(body-notes)* allow clearing the workout link and severity
- *(workouts)* reject a shift that moves the finish into the future
- *(search)* weight cardio averages by duration
- *(sets)* shape batch results by argument and reject a shared performed_at
- *(sets)* treat a standard variant as the last variant used here
- *(search)* report the per-side weight with the best e1RM
- *(sets)* accept load_basis only on unilateral sets and allow clearing it
- *(search)* compare unilateral weights per side in top weight and records
- *(gyms)* reject a blank equipment or notes append
- *(search)* count warm-up and interrupted cardio toward weekly cardio

### Other

- *(search)* check coarse groups are absent from muscle rows
- *(time)* move the optional past-time parser next to parse_time
- *(sets)* name the km per mile factor
- *(sets)* cover the set_ids count limit
- *(sets)* cover the last-session fallback for an exercise new to this gym
- *(migrations)* fail when two migrations share a number
- Merge pull request #41 from StephanMeijer/feat/set-type

## [0.3.1](https://github.com/StephanMeijer/gym-mcp/compare/v0.3.0...v0.3.1) - 2026-11-02

### Added

- *(measurements)* waist, body fat, resting heart rate and blood pressure
- *(measurements)* track body height
- *(measurements)* track body weight over time

### Other

- *(measurements)* rename height kind to body_height

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
