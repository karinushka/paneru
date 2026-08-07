//! Moving [`ScriptValue`] in and out of Lua.
//!
//! Hand-written rather than routed through mlua's serde bridge: serde spells
//! an enum as its discriminant, so `ScriptValue::Int(5)` would arrive in Lua
//! as `{ Int = 5 }` rather than as `5`.
//!
//! Behind the `lua` feature, so a client wanting only the wire types links no
//! interpreter.

use std::collections::BTreeMap;

use mlua::prelude::*;

use crate::script_value::ScriptValue;

impl IntoLua for ScriptValue {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        Ok(match self {
            Self::Null => LuaValue::Nil,
            Self::Bool(value) => LuaValue::Boolean(value),
            Self::Int(value) => LuaValue::Integer(value),
            Self::Float(value) => LuaValue::Number(value),
            Self::Str(value) => LuaValue::String(lua.create_string(&value)?),
            Self::List(values) => {
                let table = lua.create_table()?;
                for (index, value) in values.into_iter().enumerate() {
                    // Lua sequences start at 1.
                    table.set(index + 1, value.into_lua(lua)?)?;
                }
                LuaValue::Table(table)
            }
            Self::Map(entries) => {
                let table = lua.create_table()?;
                for (key, value) in entries {
                    table.set(key, value.into_lua(lua)?)?;
                }
                LuaValue::Table(table)
            }
        })
    }
}

impl FromLua for ScriptValue {
    fn from_lua(value: LuaValue, lua: &Lua) -> LuaResult<Self> {
        Ok(match value {
            LuaValue::Nil => Self::Null,
            LuaValue::Boolean(value) => Self::Bool(value),
            LuaValue::Integer(value) => Self::Int(value),
            LuaValue::Number(value) => Self::Float(value),
            LuaValue::String(value) => Self::Str(value.to_str()?.to_string()),
            LuaValue::Table(table) => table_to_value(&table, lua)?,
            other => {
                return Err(LuaError::RuntimeError(format!(
                    "cannot store a {} in paneru.state",
                    other.type_name()
                )));
            }
        })
    }
}

/// Decides whether a Lua table is an array or a dictionary, and converts it.
/// A table is treated as a list when its keys are exactly `1..=n` (an empty
/// table counts as a list).
fn table_to_value(table: &LuaTable, lua: &Lua) -> LuaResult<ScriptValue> {
    let length = table.raw_len();
    let mut keys = 0usize;
    for pair in table.clone().pairs::<LuaValue, LuaValue>() {
        pair?;
        keys += 1;
    }

    if keys == length {
        let mut values = Vec::with_capacity(length);
        for index in 1..=length {
            values.push(ScriptValue::from_lua(table.get(index)?, lua)?);
        }
        return Ok(ScriptValue::List(values));
    }

    let mut entries = BTreeMap::new();
    for pair in table.clone().pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair?;
        // Numeric keys in a mixed table become their decimal spelling, since a
        // map's keys are strings; this is the same thing a JSON encoder does.
        let key = match key {
            LuaValue::String(key) => key.to_str()?.to_string(),
            LuaValue::Integer(key) => key.to_string(),
            other => {
                return Err(LuaError::RuntimeError(format!(
                    "paneru.state keys must be strings, got {}",
                    other.type_name()
                )));
            }
        };
        entries.insert(key, ScriptValue::from_lua(value, lua)?);
    }
    Ok(ScriptValue::Map(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored value must come back to a script as the thing it stored, not as
    /// the tagged shape the wire uses.
    #[test]
    fn values_round_trip_as_plain_lua() {
        let lua = Lua::new();

        let cases = [
            ScriptValue::Null,
            ScriptValue::Bool(true),
            ScriptValue::Int(-7),
            ScriptValue::Str("hello".to_string()),
            ScriptValue::List(vec![ScriptValue::Int(1), ScriptValue::Int(2)]),
            ScriptValue::Map(BTreeMap::from([(
                "open".to_string(),
                ScriptValue::Bool(false),
            )])),
        ];

        for case in cases {
            let value = case.clone().into_lua(&lua).expect("into lua");
            let back = ScriptValue::from_lua(value, &lua).expect("from lua");
            assert_eq!(back, case);
        }
    }

    #[test]
    fn an_integer_arrives_as_a_number_not_a_table() {
        let lua = Lua::new();
        let value = ScriptValue::Int(42).into_lua(&lua).expect("into lua");
        assert!(matches!(value, LuaValue::Integer(42)), "got {value:?}");
    }

    #[test]
    fn a_sequence_is_a_list_and_a_record_is_a_map() {
        let lua = Lua::new();

        let list: LuaValue = lua.load(r#"return {"a", "b"}"#).eval().expect("eval");
        assert_eq!(
            ScriptValue::from_lua(list, &lua).expect("from lua"),
            ScriptValue::List(vec![
                ScriptValue::Str("a".to_string()),
                ScriptValue::Str("b".to_string())
            ])
        );

        let record: LuaValue = lua.load("return {open = true}").eval().expect("eval");
        assert_eq!(
            ScriptValue::from_lua(record, &lua).expect("from lua"),
            ScriptValue::Map(BTreeMap::from([(
                "open".to_string(),
                ScriptValue::Bool(true)
            )]))
        );
    }
}
