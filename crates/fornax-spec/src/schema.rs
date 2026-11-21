// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! JSON Schema (draft 2020-12) for `.fornax/` files, for editor completion and checks.
//!
//! Hand-written rather than derived with `schemars`: the loader does not use serde (it
//! walks a positioned YAML tree to report precise diagnostics), so deriving would add a
//! dependency and serde derives only for the schema, and the derived output could not
//! express our shorthands (`on: push`, single-string lists) or name/label patterns. A unit
//! test keeps the schema's property names in sync with the loader's field lists.
//!
//! The schema is a superset check for editors; [`crate::load`] remains authoritative
//! (needs/cycles, cron syntax, expressions and patterns are only checked there).

use serde_json::{Value, json};

/// Schema `$id`.
pub const SCHEMA_ID: &str = "https://fornax.dev/schema/fornax-v1.json";

fn string_list() -> Value {
    json!({
        "oneOf": [
            { "type": "string" },
            { "type": "array", "items": { "type": "string" } }
        ]
    })
}

fn env_map() -> Value {
    json!({
        "type": "object",
        "propertyNames": { "pattern": "^[A-Za-z_][A-Za-z0-9_]*$" },
        "additionalProperties": { "type": ["string", "number", "boolean", "null"] }
    })
}

fn timeout() -> Value {
    json!({ "type": "integer", "minimum": 1, "maximum": 1440 })
}

fn label() -> Value {
    json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,64}$" })
}

fn name() -> Value {
    json!({ "type": "string", "pattern": "^[A-Za-z0-9_.()=,-]([A-Za-z0-9 _.()=,-]{0,98}[A-Za-z0-9_.()=,-])?$" })
}

fn push_def() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": false,
        "properties": {
            "branches": { "$ref": "#/$defs/stringList" },
            "branches-ignore": { "$ref": "#/$defs/stringList" },
            "tags": { "$ref": "#/$defs/stringList" },
            "tags-ignore": { "$ref": "#/$defs/stringList" },
            "paths": { "$ref": "#/$defs/stringList" },
            "paths-ignore": { "$ref": "#/$defs/stringList" }
        }
    })
}

fn pull_request_def() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": false,
        "properties": {
            "branches": { "$ref": "#/$defs/stringList" },
            "branches-ignore": { "$ref": "#/$defs/stringList" },
            "paths": { "$ref": "#/$defs/stringList" },
            "paths-ignore": { "$ref": "#/$defs/stringList" }
        }
    })
}

fn on_def() -> Value {
    let event = json!({ "enum": ["push", "pull_request", "manual"] });
    json!({
        "oneOf": [
            event.clone(),
            { "type": "array", "items": event },
            {
                "type": "object",
                "additionalProperties": false,
                "minProperties": 1,
                "properties": {
                    "push": { "$ref": "#/$defs/push" },
                    "pull_request": { "$ref": "#/$defs/pullRequest" },
                    "schedule": {
                        "description": "Cron expressions (5 fields or @daily etc.), evaluated in UTC.",
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string" }
                    },
                    "manual": {
                        "type": ["object", "null", "boolean"],
                        "additionalProperties": false
                    }
                }
            }
        ]
    })
}

fn step_def() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["run"],
        "properties": {
            "name": { "type": "string" },
            "run": { "type": "string", "minLength": 1 },
            "env": { "$ref": "#/$defs/env" },
            "timeout-minutes": { "$ref": "#/$defs/timeout" },
            "working-directory": { "type": "string" }
        }
    })
}

fn job_def() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["steps"],
        "properties": {
            "runs-on": {
                "oneOf": [
                    { "$ref": "#/$defs/label" },
                    { "type": "array", "minItems": 1, "items": { "$ref": "#/$defs/label" } }
                ]
            },
            "needs": { "$ref": "#/$defs/stringList" },
            "if": {
                "description": "Expression, bare or in ${{ }}: context event/ref/branch/tag/sha/repo/vars.NAME/matrix.NAME, == != && || !, startsWith/endsWith/contains/matches.",
                "type": ["string", "boolean"]
            },
            "env": { "$ref": "#/$defs/env" },
            "timeout-minutes": { "$ref": "#/$defs/timeout" },
            "steps": { "type": "array", "minItems": 1, "maxItems": 100, "items": { "$ref": "#/$defs/step" } },
            "strategy": {
                "type": "object",
                "additionalProperties": false,
                "required": ["matrix"],
                "properties": {
                    "matrix": {
                        "type": "object",
                        "minProperties": 1,
                        "propertyNames": { "pattern": "^[A-Za-z_][A-Za-z0-9_-]*$", "not": { "enum": ["include", "exclude"] } },
                        "additionalProperties": {
                            "type": "array",
                            "minItems": 1,
                            "uniqueItems": true,
                            "items": { "type": ["string", "number", "boolean"] }
                        }
                    },
                    "max-parallel": { "type": "integer", "minimum": 1, "maximum": 256 }
                }
            },
            "artifacts": {
                "type": "object",
                "additionalProperties": false,
                "required": ["paths"],
                "properties": {
                    "paths": { "$ref": "#/$defs/stringList" },
                    "expire-in": { "type": "string", "pattern": "^[0-9]+[smhdw]$" }
                }
            },
            "environment": { "$ref": "#/$defs/label" },
            "trigger-deployment": { "type": "string" }
        }
    })
}

fn pipeline_def() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "version", "on", "jobs"],
        "properties": {
            "kind": { "const": "Pipeline" },
            "version": { "const": 1 },
            "name": { "$ref": "#/$defs/name" },
            "on": { "$ref": "#/$defs/on" },
            "variables": { "$ref": "#/$defs/env" },
            "jobs": {
                "type": "object",
                "minProperties": 1,
                "propertyNames": { "$ref": "#/$defs/name" },
                "additionalProperties": { "$ref": "#/$defs/job" }
            }
        }
    })
}

fn deployment_def() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "version", "environment", "source", "target"],
        "properties": {
            "kind": { "const": "Deployment" },
            "version": { "const": 1 },
            "name": { "$ref": "#/$defs/name" },
            "environment": { "$ref": "#/$defs/label" },
            "source": {
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": { "path": { "type": "string", "minLength": 1 } }
            },
            "target": {
                "type": "object",
                "additionalProperties": false,
                "required": ["type"],
                "properties": {
                    "type": { "enum": ["host", "docker-compose", "kubernetes"] },
                    "host": { "$ref": "#/$defs/label" },
                    "namespace": { "$ref": "#/$defs/label" },
                    "project": { "$ref": "#/$defs/label" }
                }
            },
            "sync": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "auto": { "type": "boolean", "default": false },
                    "prune": { "type": "boolean", "default": false },
                    "self-heal": { "type": "boolean", "default": false }
                }
            },
            "promotion": {
                "type": "object",
                "additionalProperties": false,
                "required": ["to"],
                "properties": {
                    "to": { "$ref": "#/$defs/label" },
                    "requires-approval": { "type": "boolean", "default": false }
                }
            }
        }
    })
}

/// The JSON Schema for `.fornax/*.yaml` (Pipeline and Deployment, `version: 1`).
pub fn json_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": SCHEMA_ID,
        "title": "Fornax .fornax/ file (version 1)",
        "oneOf": [
            { "$ref": "#/$defs/pipeline" },
            { "$ref": "#/$defs/deployment" }
        ],
        "$defs": {
            "pipeline": pipeline_def(),
            "deployment": deployment_def(),
            "on": on_def(),
            "push": push_def(),
            "pullRequest": pull_request_def(),
            "job": job_def(),
            "step": step_def(),
            "stringList": string_list(),
            "env": env_map(),
            "timeout": timeout(),
            "label": label(),
            "name": name()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    fn props(v: &Value) -> Vec<String> {
        let mut keys: Vec<String> = v["properties"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    fn sorted(fields: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn schema_fields_match_loader() {
        let s = json_schema();
        let d = &s["$defs"];
        assert_eq!(props(&d["pipeline"]), sorted(load::PIPELINE_FIELDS));
        assert_eq!(props(&d["deployment"]), sorted(load::DEPLOYMENT_FIELDS));
        assert_eq!(props(&d["job"]), sorted(load::JOB_FIELDS));
        assert_eq!(props(&d["step"]), sorted(load::STEP_FIELDS));
        assert_eq!(props(&d["push"]), sorted(load::PUSH_FIELDS));
        assert_eq!(props(&d["pullRequest"]), sorted(load::PULL_REQUEST_FIELDS));
        assert_eq!(props(&d["on"]["oneOf"][2]), sorted(load::ON_FIELDS));
        assert_eq!(
            props(&d["job"]["properties"]["strategy"]),
            sorted(load::STRATEGY_FIELDS)
        );
        assert_eq!(
            props(&d["job"]["properties"]["artifacts"]),
            sorted(load::ARTIFACT_FIELDS)
        );
        let dp = &d["deployment"]["properties"];
        assert_eq!(props(&dp["source"]), sorted(load::SOURCE_FIELDS));
        assert_eq!(props(&dp["target"]), sorted(load::TARGET_FIELDS));
        assert_eq!(props(&dp["sync"]), sorted(load::SYNC_FIELDS));
        assert_eq!(props(&dp["promotion"]), sorted(load::PROMOTION_FIELDS));
    }

    #[test]
    fn every_ref_resolves() {
        fn walk(v: &Value, root: &Value) {
            match v {
                Value::Object(o) => {
                    if let Some(Value::String(r)) = o.get("$ref") {
                        let name = r.strip_prefix("#/$defs/").expect("local ref");
                        assert!(root["$defs"].get(name).is_some(), "dangling {r}");
                    }
                    o.values().for_each(|x| walk(x, root));
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, root)),
                _ => {}
            }
        }
        let s = json_schema();
        walk(&s, &s);
    }
}
