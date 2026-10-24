//! Typed signatures (#3): read from ONNX, checked against a binding to a
//! dataset schema or object type, and compared between versions.
//!
//! Hinagata makes no runtime network calls, so a binding carries the target's
//! fields inline. Each model input takes one field (a column or property),
//! or, for a `[batch, n]` tensor, `n` fields in order.

use crate::registry::{Signature, TensorSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod onnx {
    tonic::include_proto!("onnx");
}

/// ONNX element types by number, as Hinagata names them.
fn onnx_dtype(elem_type: i32) -> Option<&'static str> {
    Some(match elem_type {
        1 => "float32",
        2 => "uint8",
        3 => "int8",
        4 => "uint16",
        5 => "int16",
        6 => "int32",
        7 => "int64",
        8 => "string",
        9 => "bool",
        10 => "float16",
        11 => "float64",
        12 => "uint32",
        13 => "uint64",
        _ => return None,
    })
}

/// A graph input or output, with what the graph says about it. A part the
/// graph leaves open or a signature cannot express (an unknown rank, a
/// sequence, an unknown element type) is `None`, with the reason in `why`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphValue {
    pub name: String,
    /// A plain tensor, which cannot be missing; only an optional type can.
    pub tensor: bool,
    pub dtype: Option<String>,
    pub shape: Option<Vec<i64>>,
    pub why: Vec<String>,
    /// The graph states a type a signature cannot express (a sequence, a
    /// map, an optional, an element type such as bfloat16): no declaration
    /// can match it.
    pub unsupported: Option<String>,
}

impl GraphValue {
    /// The full spec, when the graph says everything about it.
    pub fn spec(&self) -> Result<TensorSpec, String> {
        match (&self.dtype, &self.shape) {
            (Some(dtype), Some(shape)) => Ok(TensorSpec {
                name: self.name.clone(),
                dtype: dtype.clone(),
                shape: shape.clone(),
                nullable: false,
            }),
            _ => Err(format!("`{}`: {}", self.name, self.why.join(", "))),
        }
    }
}

/// Errors from a graph past the reader's limits start with this, so a caller
/// can tell them from a file that is not an ONNX model.
pub(crate) const LIMIT_ERROR: &str = "the ONNX graph is past the reader's limits: ";

/// Most graph inputs and outputs, weight initializers, and dimensions per
/// tensor the reader accepts. A file past these is not read further, so a
/// small file of many empty messages cannot exhaust memory.
const MAX_VALUES: usize = 1024;
const MAX_INITIALIZERS: usize = 1_000_000;
const MAX_DIMS: usize = 64;

enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// Visit the fields of one protobuf message without decoding it whole.
fn each_field<'a>(
    mut buf: &'a [u8],
    mut visit: impl FnMut(u32, Wire<'a>) -> Result<(), String>,
) -> Result<(), String> {
    use prost::encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field};
    let bad = |error: prost::DecodeError| format!("not an ONNX model: {error}");
    while !buf.is_empty() {
        let (tag, wire) = decode_key(&mut buf).map_err(bad)?;
        match wire {
            WireType::Varint => visit(tag, Wire::Varint(decode_varint(&mut buf).map_err(bad)?))?,
            WireType::LengthDelimited => {
                let len = decode_varint(&mut buf).map_err(bad)? as usize;
                if len > buf.len() {
                    return Err("not an ONNX model: a field runs past the end".into());
                }
                let (field, rest) = buf.split_at(len);
                buf = rest;
                visit(tag, Wire::Bytes(field))?;
            }
            other => skip_field(other, tag, &mut buf, DecodeContext::default()).map_err(bad)?,
        }
    }
    Ok(())
}

fn text(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|_| "not an ONNX model: a name is not UTF-8".to_string())
}

/// One ValueInfoProto: name (1) and type (2), whose tensor type (1) holds
/// the element type (1) and shape (2) of dimensions (1) with a size (1) or
/// symbol (2).
fn read_value(bytes: &[u8]) -> Result<GraphValue, String> {
    let mut name = "";
    let mut tensor: Option<&[u8]> = None;
    let mut typed = false;
    each_field(bytes, |tag, wire| {
        match (tag, wire) {
            (1, Wire::Bytes(field)) => name = text(field)?,
            (2, Wire::Bytes(field)) => {
                typed = true;
                each_field(field, |tag, wire| {
                    if let (1, Wire::Bytes(field)) = (tag, wire) {
                        tensor = Some(field);
                    }
                    Ok(())
                })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let mut why = Vec::new();
    let (mut dtype, mut shape, mut unsupported) = (None, None, None);
    match tensor {
        None if typed => {
            why.push("it is not a plain tensor".to_string());
            unsupported = Some(format!("`{name}` is not a plain tensor"));
        }
        None => why.push("it has no type".to_string()),
        Some(tensor) => {
            let mut elem_type = 0;
            let mut dims: Option<Vec<i64>> = None;
            each_field(tensor, |tag, wire| {
                match (tag, wire) {
                    (1, Wire::Varint(value)) => elem_type = value as i32,
                    (2, Wire::Bytes(field)) => {
                        let found = dims.get_or_insert_with(Vec::new);
                        each_field(field, |tag, wire| {
                            let (1, Wire::Bytes(dim)) = (tag, wire) else {
                                return Ok(());
                            };
                            if found.len() == MAX_DIMS {
                                return Err(format!(
                                    "{LIMIT_ERROR}`{name}` has more than {MAX_DIMS} dimensions"
                                ));
                            }
                            // A concrete size, zero included; a symbol or a
                            // missing size is -1.
                            let mut size = -1;
                            each_field(dim, |tag, wire| {
                                if let (1, Wire::Varint(value)) = (tag, wire) {
                                    size = (value as i64).max(-1);
                                }
                                Ok(())
                            })?;
                            found.push(size);
                            Ok(())
                        })?;
                    }
                    _ => {}
                }
                Ok(())
            })?;
            dtype = onnx_dtype(elem_type).map(str::to_string);
            if dtype.is_none() {
                why.push(format!("element type {elem_type} has no signature dtype"));
                // 0 is ONNX's "undefined": the graph leaves it open.
                if elem_type != 0 {
                    unsupported = Some(format!(
                        "`{name}` has element type {elem_type}, which a signature cannot express"
                    ));
                }
            }
            // An absent shape is an unknown rank.
            if dims.is_none() {
                why.push("its rank is unknown".to_string());
            }
            shape = dims;
        }
    }
    Ok(GraphValue {
        name: name.to_string(),
        tensor: tensor.is_some(),
        dtype,
        shape,
        why,
        unsupported,
    })
}

/// The inputs (without weight initializers, which older exporters list as
/// inputs) and outputs of an ONNX model's graph. `Err` when the bytes are
/// not an ONNX model with a graph, or one past the reader's limits.
pub(crate) fn read_graph(bytes: &[u8]) -> Result<(Vec<GraphValue>, Vec<GraphValue>), String> {
    let mut graph: Option<&[u8]> = None;
    each_field(bytes, |tag, wire| {
        if let (7, Wire::Bytes(field)) = (tag, wire) {
            graph = Some(field);
        }
        Ok(())
    })?;
    let graph = graph.ok_or("the ONNX model has no graph")?;
    let mut weights = std::collections::HashSet::new();
    let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
    each_field(graph, |tag, wire| {
        let Wire::Bytes(field) = wire else {
            return Ok(());
        };
        match tag {
            5 => {
                if weights.len() == MAX_INITIALIZERS {
                    return Err(format!(
                        "{LIMIT_ERROR}more than {MAX_INITIALIZERS} initializers"
                    ));
                }
                each_field(field, |tag, wire| {
                    if let (8, Wire::Bytes(name)) = (tag, wire) {
                        weights.insert(text(name)?);
                    }
                    Ok(())
                })?;
            }
            // Sparse initializers keep their name in `values` (1).
            15 => {
                if weights.len() == MAX_INITIALIZERS {
                    return Err(format!(
                        "{LIMIT_ERROR}more than {MAX_INITIALIZERS} initializers"
                    ));
                }
                each_field(field, |tag, wire| {
                    if let (1, Wire::Bytes(values)) = (tag, wire) {
                        each_field(values, |tag, wire| {
                            if let (8, Wire::Bytes(name)) = (tag, wire) {
                                weights.insert(text(name)?);
                            }
                            Ok(())
                        })?;
                    }
                    Ok(())
                })?;
            }
            11 | 12 => {
                if inputs.len() + outputs.len() == MAX_VALUES {
                    return Err(format!(
                        "{LIMIT_ERROR}more than {MAX_VALUES} inputs and outputs"
                    ));
                }
                if tag == 11 {
                    inputs.push(field)
                } else {
                    outputs.push(field)
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    let mut read = Vec::new();
    for field in inputs {
        let value = read_value(field)?;
        if !weights.contains(value.name.as_str()) {
            read.push(value);
        }
    }
    if read.is_empty() {
        return Err("the ONNX graph declares no inputs".into());
    }
    // An output may name an initializer; it is still an output.
    let outputs = outputs
        .into_iter()
        .map(read_value)
        .collect::<Result<_, _>>()?;
    Ok((read, outputs))
}

/// Check a declared signature against the graph: the same input and output
/// names, and every type and shape the graph states equal. What the graph
/// leaves open is taken as declared.
pub(crate) fn verify_against_graph(
    declared: &Signature,
    graph: &(Vec<GraphValue>, Vec<GraphValue>),
) -> Result<(), String> {
    for (role, specs, values) in [
        ("input", &declared.inputs, &graph.0),
        ("output", &declared.outputs, &graph.1),
    ] {
        let declared_names: std::collections::BTreeSet<&String> =
            specs.iter().map(|spec| &spec.name).collect();
        let graph_names: std::collections::BTreeSet<&String> =
            values.iter().map(|value| &value.name).collect();
        if declared_names != graph_names {
            return Err(format!(
                "the declared {role}s {declared_names:?} are not the graph's {graph_names:?}"
            ));
        }
        for value in values {
            let Some(spec) = specs.iter().find(|spec| spec.name == value.name) else {
                continue;
            };
            if value
                .dtype
                .as_ref()
                .is_some_and(|dtype| *dtype != spec.dtype)
            {
                return Err(format!(
                    "{role} `{}` is {} in the graph",
                    value.name,
                    value.dtype.as_deref().unwrap_or_default()
                ));
            }
            if let Some(why) = &value.unsupported {
                return Err(format!("{role} {why}"));
            }
            if value.tensor && spec.nullable {
                return Err(format!(
                    "{role} `{}` is a plain tensor in the graph and cannot be null",
                    value.name
                ));
            }
            if value
                .shape
                .as_ref()
                .is_some_and(|shape| *shape != spec.shape)
            {
                return Err(format!(
                    "{role} `{}` has shape {:?} in the graph",
                    value.name,
                    value.shape.as_deref().unwrap_or_default()
                ));
            }
        }
    }
    Ok(())
}

/// The signature an ONNX model declares, if the graph says all of it.
#[cfg(test)]
fn from_onnx(bytes: &[u8]) -> Result<Signature, String> {
    let (inputs, outputs) = read_graph(bytes)?;
    let specs = |values: Vec<GraphValue>| {
        values
            .iter()
            .map(GraphValue::spec)
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(Signature {
        inputs: specs(inputs)?,
        outputs: specs(outputs)?,
    })
}

/// A field of the target a version is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub dtype: String,
    pub nullable: bool,
}

/// What a version reads its inputs from: a dataset schema or an object
/// type's properties, given inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// `dataset` or `object_type`.
    pub target_kind: String,
    pub target: String,
    pub fields: Vec<Field>,
    /// The fields each input takes, in order. An input not listed takes the
    /// field of its own name.
    pub inputs: BTreeMap<String, Vec<String>>,
}

/// Whether a field of `from` can feed an input of `to` without loss.
fn widens(from: &str, to: &str) -> bool {
    const SIGNED: &[&str] = &["int8", "int16", "int32", "int64"];
    const UNSIGNED: &[&str] = &["uint8", "uint16", "uint32", "uint64"];
    const FLOATS: &[&str] = &["float16", "float32", "float64"];
    let rank = |family: &[&str], dtype: &str| family.iter().position(|item| *item == dtype);
    if from == to {
        return true;
    }
    for family in [SIGNED, UNSIGNED, FLOATS] {
        if let (Some(a), Some(b)) = (rank(family, from), rank(family, to)) {
            return a <= b;
        }
    }
    // Unsigned integers fit in wider signed ones, and small integers in
    // floats whose mantissa covers their whole range.
    matches!(
        (from, to),
        ("uint8", "int16" | "int32" | "int64")
            | ("uint16", "int32" | "int64")
            | ("uint32", "int64")
            | ("int8" | "uint8", "float16")
            | ("int8" | "int16" | "uint8" | "uint16", "float32" | "float64")
            | ("int32" | "uint32", "float64")
    )
}

/// Every way `signature` does not fit `binding`; empty when it fits.
pub(crate) fn check_binding(signature: &Signature, binding: &Binding) -> Vec<String> {
    let mut problems = Vec::new();
    if !matches!(binding.target_kind.as_str(), "dataset" | "object_type") {
        problems.push(format!(
            "binding target_kind `{}` must be dataset or object_type",
            binding.target_kind
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for field in &binding.fields {
        if !seen.insert(field.name.as_str()) {
            problems.push(format!("field `{}` is listed twice", field.name));
        }
    }
    let fields: BTreeMap<&str, &Field> = binding
        .fields
        .iter()
        .map(|field| (field.name.as_str(), field))
        .collect();
    for name in binding.inputs.keys() {
        if !signature.inputs.iter().any(|input| &input.name == name) {
            problems.push(format!(
                "binding names input `{name}`, which the model does not have"
            ));
        }
    }
    for input in &signature.inputs {
        let taken = binding
            .inputs
            .get(&input.name)
            .cloned()
            .unwrap_or_else(|| vec![input.name.clone()]);
        let width = match input.shape.as_slice() {
            [] | [_] => Some(1),
            [_, width] if *width >= 0 => Some(*width as usize),
            [_, _] => None,
            _ => {
                problems.push(format!(
                    "input `{}` has shape {:?}; only [batch] and [batch, n] inputs bind to fields",
                    input.name, input.shape
                ));
                continue;
            }
        };
        if width.is_some_and(|width| width != taken.len()) {
            problems.push(format!(
                "input `{}` takes {} fields, the binding gives {}",
                input.name,
                width.unwrap_or_default(),
                taken.len()
            ));
        }
        for name in &taken {
            let Some(field) = fields.get(name.as_str()) else {
                problems.push(format!(
                    "input `{}` needs field `{name}`, which {} `{}` does not have",
                    input.name, binding.target_kind, binding.target
                ));
                continue;
            };
            if !widens(&field.dtype, &input.dtype) {
                problems.push(format!(
                    "field `{name}` is {}, input `{}` takes {}",
                    field.dtype, input.name, input.dtype
                ));
            }
            if field.nullable && !input.nullable {
                problems.push(format!(
                    "field `{name}` may be null, input `{}` may not",
                    input.name
                ));
            }
        }
    }
    problems
}

/// Changes from `old` to `new` that break a caller of `old`: a new input,
/// an input that takes another type or shape or no longer accepts nulls, and
/// a removed output, one of another type or shape, or one that may now be
/// null.
pub(crate) fn breaking_changes(old: &Signature, new: &Signature) -> Vec<String> {
    let find =
        |specs: &[TensorSpec], name: &str| specs.iter().find(|spec| spec.name == name).cloned();
    let mut breaking = Vec::new();
    for input in &new.inputs {
        match find(&old.inputs, &input.name) {
            None => breaking.push(format!("adds input `{}`", input.name)),
            Some(before) => {
                if before.dtype != input.dtype || before.shape != input.shape {
                    breaking.push(format!(
                        "input `{}` changes from {} {:?} to {} {:?}",
                        input.name, before.dtype, before.shape, input.dtype, input.shape
                    ));
                }
                if before.nullable && !input.nullable {
                    breaking.push(format!("input `{}` no longer accepts null", input.name));
                }
            }
        }
    }
    for output in &old.outputs {
        match find(&new.outputs, &output.name) {
            None => breaking.push(format!("removes output `{}`", output.name)),
            Some(after) => {
                if after.dtype != output.dtype || after.shape != output.shape {
                    breaking.push(format!(
                        "output `{}` changes from {} {:?} to {} {:?}",
                        output.name, output.dtype, output.shape, after.dtype, after.shape
                    ));
                }
                if after.nullable && !output.nullable {
                    breaking.push(format!("output `{}` may now be null", output.name));
                }
            }
        }
    }
    breaking
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn value(name: &str, elem_type: i32, dims: &[i64]) -> onnx::ValueInfoProto {
        use onnx::tensor_shape_proto::{Dimension, dimension};
        onnx::ValueInfoProto {
            name: name.into(),
            r#type: Some(onnx::TypeProto {
                value: Some(onnx::type_proto::Value::TensorType(
                    onnx::type_proto::Tensor {
                        elem_type,
                        shape: Some(onnx::TensorShapeProto {
                            dim: dims
                                .iter()
                                .map(|dim| Dimension {
                                    value: Some(if *dim > 0 {
                                        dimension::Value::DimValue(*dim)
                                    } else {
                                        dimension::Value::DimParam("batch".into())
                                    }),
                                })
                                .collect(),
                        }),
                    },
                )),
            }),
        }
    }

    /// An ONNX model with a `[batch, 3]` float input, a weight listed as an
    /// input, and an int64 label output.
    pub fn model_bytes() -> Vec<u8> {
        onnx::ModelProto {
            graph: Some(onnx::GraphProto {
                initializer: vec![onnx::TensorProto {
                    name: "weights".into(),
                }],
                input: vec![value("float_input", 1, &[-1, 3]), value("weights", 1, &[3])],
                output: vec![value("label", 7, &[-1])],
            }),
        }
        .encode_to_vec()
    }

    fn tabular(name: &str, dtype: &str, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: dtype.into(),
            nullable,
        }
    }

    #[test]
    fn the_onnx_graph_gives_the_signature() {
        let signature = from_onnx(&model_bytes()).unwrap();
        assert_eq!(signature.inputs.len(), 1);
        assert_eq!(
            (
                signature.inputs[0].dtype.as_str(),
                signature.inputs[0].shape.clone()
            ),
            ("float32", vec![-1, 3])
        );
        assert_eq!(signature.outputs[0].dtype, "int64");
        assert!(from_onnx(b"not a model").is_err());
    }

    #[test]
    fn bindings_fit_by_name_width_type_and_nullability() {
        let signature = from_onnx(&model_bytes()).unwrap();
        let binding = |fields: Vec<Field>| Binding {
            target_kind: "dataset".into(),
            target: "customers".into(),
            fields,
            inputs: BTreeMap::from([(
                "float_input".into(),
                vec!["age".into(), "tenure".into(), "spend".into()],
            )]),
        };
        let good = binding(vec![
            tabular("age", "int16", false),
            tabular("tenure", "float32", false),
            tabular("spend", "float16", false),
        ]);
        assert_eq!(check_binding(&signature, &good), Vec::<String>::new());
        let mut twice = good.clone();
        twice.fields.push(tabular("age", "float32", false));
        assert!(check_binding(&signature, &twice)[0].contains("listed twice"));
        assert!(widens("uint8", "int16") && widens("uint16", "int32") && widens("uint32", "int64"));
        assert!(!widens("uint32", "int32") && !widens("int64", "float64"));
        let bad = binding(vec![
            tabular("age", "int64", false),
            tabular("tenure", "float32", true),
            tabular("region", "string", false),
        ]);
        let problems = check_binding(&signature, &bad).join("\n");
        for expected in [
            "field `age` is int64, input `float_input` takes float32",
            "field `tenure` may be null",
            "needs field `spend`",
        ] {
            assert!(problems.contains(expected), "{expected}: {problems}");
        }
        let narrow = Binding {
            inputs: BTreeMap::from([("float_input".into(), vec!["age".into()])]),
            ..good
        };
        assert!(check_binding(&signature, &narrow)[0].contains("takes 3 fields"));
    }

    #[test]
    fn declarations_are_checked_against_what_the_graph_expresses() {
        let mut unknown_rank = value("extra", 1, &[]);
        if let Some(onnx::type_proto::Value::TensorType(tensor)) = unknown_rank
            .r#type
            .as_mut()
            .and_then(|ty| ty.value.as_mut())
        {
            tensor.shape = None;
        }
        let bytes = onnx::ModelProto {
            graph: Some(onnx::GraphProto {
                initializer: Vec::new(),
                input: vec![value("float_input", 1, &[-1, 3]), unknown_rank],
                output: vec![value("label", 7, &[-1])],
            }),
        }
        .encode_to_vec();
        // The graph cannot express `extra`, so no signature is read from it.
        assert!(from_onnx(&bytes).unwrap_err().contains("rank is unknown"));
        let graph = read_graph(&bytes).unwrap();
        let spec = |name: &str, dtype: &str, shape: Vec<i64>| TensorSpec {
            name: name.into(),
            dtype: dtype.into(),
            shape,
            nullable: false,
        };
        let declared = Signature {
            inputs: vec![
                spec("float_input", "float32", vec![-1, 3]),
                spec("extra", "float32", vec![-1, 2]),
            ],
            outputs: vec![spec("label", "int64", vec![-1])],
        };
        assert_eq!(verify_against_graph(&declared, &graph), Ok(()));
        let mut wrong_type = declared.clone();
        wrong_type.inputs[0].dtype = "float64".into();
        assert!(verify_against_graph(&wrong_type, &graph).is_err());
        let mut nullable = declared.clone();
        nullable.inputs[0].nullable = true;
        assert!(
            verify_against_graph(&nullable, &graph)
                .unwrap_err()
                .contains("cannot be null")
        );
        // A type the graph states but a signature cannot express matches
        // no declaration.
        let bfloat = onnx::ModelProto {
            graph: Some(onnx::GraphProto {
                initializer: Vec::new(),
                input: vec![value("float_input", 16, &[-1, 3])],
                output: vec![value("label", 7, &[-1])],
            }),
        }
        .encode_to_vec();
        let bfloat_graph = read_graph(&bfloat).unwrap();
        let declared_float = Signature {
            inputs: vec![spec("float_input", "float32", vec![-1, 3])],
            outputs: vec![spec("label", "int64", vec![-1])],
        };
        assert!(
            verify_against_graph(&declared_float, &bfloat_graph)
                .unwrap_err()
                .contains("element type 16")
        );
        // The rank is open, but the element type is not.
        let mut wrong_open = declared.clone();
        wrong_open.inputs[1].dtype = "string".into();
        assert!(verify_against_graph(&wrong_open, &graph).is_err());
        let mut missing = declared.clone();
        missing.inputs.pop();
        assert!(verify_against_graph(&missing, &graph).is_err());
    }

    #[test]
    fn graphs_past_the_reader_limits_are_refused() {
        // Many empty inputs in a few bytes each.
        let input = onnx::ValueInfoProto::default();
        let graph = onnx::GraphProto {
            initializer: Vec::new(),
            input: vec![input; MAX_VALUES + 1],
            output: Vec::new(),
        };
        let bytes = onnx::ModelProto { graph: Some(graph) }.encode_to_vec();
        assert!(read_graph(&bytes).unwrap_err().contains("more than"));
    }

    #[test]
    fn breaking_signature_changes_are_flagged() {
        let old = from_onnx(&model_bytes()).unwrap();
        assert!(breaking_changes(&old, &old).is_empty());
        let mut new = old.clone();
        new.inputs[0].shape = vec![-1, 4];
        new.inputs.push(TensorSpec {
            name: "region".into(),
            dtype: "string".into(),
            shape: vec![-1],
            nullable: false,
        });
        new.outputs.clear();
        let breaking = breaking_changes(&old, &new).join("\n");
        for expected in [
            "input `float_input` changes",
            "adds input `region`",
            "removes output `label`",
        ] {
            assert!(breaking.contains(expected), "{expected}: {breaking}");
        }
        let mut nullable = old.clone();
        nullable.outputs[0].nullable = true;
        assert!(breaking_changes(&old, &nullable)[0].contains("may now be null"));
        // Removing an input or adding an output does not break callers.
        let mut relaxed = old.clone();
        relaxed.outputs.push(TensorSpec {
            name: "probability".into(),
            dtype: "float32".into(),
            shape: vec![-1, 2],
            nullable: false,
        });
        assert!(breaking_changes(&old, &relaxed).is_empty());
    }
}
