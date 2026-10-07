#!/usr/bin/env python3
"""Freezes Silero VAD to 16 kHz for tract: crates/vv-vad/model/silero_vad_16k.onnx.

tract cannot type the model's `If` nodes, but every one depends only on the
input shapes and on `sr`. Fixing those, each `If` is replaced by the branch
it takes, and the result is checked against the original.

Usage (in a venv with onnx, onnxruntime and numpy):
    scripts/freeze_silero_vad.py silero_vad.onnx crates/vv-vad/model/silero_vad_16k.onnx
The source model is src/silero_vad/data/silero_vad.onnx of
https://github.com/snakers4/silero-vad.
"""
import sys

import numpy as np
import onnx
import onnxruntime as ort
from onnx import helper, numpy_helper

CHUNK, CONTEXT, RATE = 512, 64, 16000

src, dst = sys.argv[1], sys.argv[2]
model = onnx.load(src)
graph = model.graph

sr = next(i for i in graph.input if i.name == "sr")
graph.input.remove(sr)
graph.initializer.append(numpy_helper.from_array(np.array(RATE, dtype=np.int64), "sr"))

feeds = {
    "input": (np.random.default_rng(0).standard_normal((1, CONTEXT + CHUNK)) * 0.1).astype(np.float32),
    "state": np.zeros((2, 1, 128), dtype=np.float32),
}


def conditions():
    probe = onnx.ModelProto()
    probe.CopyFrom(model)
    names = sorted({n.input[0] for n in probe.graph.node if n.op_type == "If"})
    for name in names:
        probe.graph.output.append(helper.make_tensor_value_info(name, onnx.TensorProto.BOOL, None))
    session = ort.InferenceSession(probe.SerializeToString(), providers=["CPUExecutionProvider"])
    return dict(zip(names, session.run(names, feeds)))


# Inlining a branch can expose the `If`s nested in it.
while any(n.op_type == "If" for n in graph.node):
    taken_by = conditions()
    nodes = []
    for node in graph.node:
        if node.op_type != "If":
            nodes.append(node)
            continue
        taken = "then_branch" if bool(np.asarray(taken_by[node.input[0]]).reshape(-1)[0]) else "else_branch"
        branch = next(a.g for a in node.attribute if a.name == taken)
        renames = {o.name: node.output[i] for i, o in enumerate(branch.output)}
        graph.initializer.extend(branch.initializer)
        for inner in branch.node:
            inner = onnx.NodeProto.FromString(inner.SerializeToString())
            inner.input[:] = [renames.get(i, i) for i in inner.input]
            inner.output[:] = [renames.get(o, o) for o in inner.output]
            nodes.append(inner)
        for out in branch.output:
            if not any(out.name in inner.output for inner in branch.node):
                nodes.append(helper.make_node("Identity", [out.name], [renames[out.name]]))
    del graph.node[:]
    graph.node.extend(nodes)

dims = next(i for i in graph.input if i.name == "input").type.tensor_type.shape.dim
dims[0].dim_value, dims[1].dim_value = 1, CONTEXT + CHUNK
del graph.value_info[:]
model = onnx.shape_inference.infer_shapes(model)
onnx.checker.check_model(model)
onnx.save(model, dst)

original = ort.InferenceSession(src, providers=["CPUExecutionProvider"])
frozen = ort.InferenceSession(dst, providers=["CPUExecutionProvider"])
rng = np.random.default_rng(1)
state_a = state_b = feeds["state"]
worst = 0.0
for k in range(50):
    x = (rng.standard_normal((1, CONTEXT + CHUNK)) * (0.3 if k % 10 < 5 else 0.01)).astype(np.float32)
    a, state_a = original.run(None, {"input": x, "state": state_a, "sr": np.array(RATE, dtype=np.int64)})
    b, state_b = frozen.run(None, {"input": x, "state": state_b})
    worst = max(worst, float(np.abs(a - b).max()))
print(f"largest difference from the original: {worst:g}")
sys.exit(0 if worst < 1e-5 else 1)
