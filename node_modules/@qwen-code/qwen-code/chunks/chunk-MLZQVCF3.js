// Force strict mode and setup for ESM
"use strict";
import {
  init_esbuild_shims
} from "./chunk-A4BMJM77.js";

// packages/core/src/output/types.ts
init_esbuild_shims();
var InputFormat = /* @__PURE__ */ ((InputFormat2) => {
  InputFormat2["TEXT"] = "text";
  InputFormat2["STREAM_JSON"] = "stream-json";
  return InputFormat2;
})(InputFormat || {});
var OutputFormat = /* @__PURE__ */ ((OutputFormat2) => {
  OutputFormat2["TEXT"] = "text";
  OutputFormat2["JSON"] = "json";
  OutputFormat2["STREAM_JSON"] = "stream-json";
  return OutputFormat2;
})(OutputFormat || {});

export {
  InputFormat,
  OutputFormat
};
/**
 * @license
 * Copyright 2025 Google LLC
 * SPDX-License-Identifier: Apache-2.0
 */
