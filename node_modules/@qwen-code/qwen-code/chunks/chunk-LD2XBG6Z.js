// Force strict mode and setup for ESM
"use strict";
import {
  init_esbuild_shims
} from "./chunk-A4BMJM77.js";
import {
  __name
} from "./chunk-J2S4EL5Y.js";

// packages/core/src/agents/team/identity.ts
init_esbuild_shims();
import { AsyncLocalStorage } from "node:async_hooks";
var teammateIdentityStore = new AsyncLocalStorage();
function getTeammateContext() {
  return teammateIdentityStore.getStore();
}
__name(getTeammateContext, "getTeammateContext");
function isInProcessTeammate() {
  return teammateIdentityStore.getStore() !== void 0;
}
__name(isInProcessTeammate, "isInProcessTeammate");
function getAgentName() {
  return teammateIdentityStore.getStore()?.agentName;
}
__name(getAgentName, "getAgentName");
function getTeamName() {
  return teammateIdentityStore.getStore()?.teamName;
}
__name(getTeamName, "getTeamName");
function resolveActiveTeamName(fallback) {
  return getTeamName() ?? fallback;
}
__name(resolveActiveTeamName, "resolveActiveTeamName");
var isTeammate = isInProcessTeammate;
function isTeamLead() {
  return teammateIdentityStore.getStore()?.isTeamLead ?? false;
}
__name(isTeamLead, "isTeamLead");
function getTeammateColor() {
  return teammateIdentityStore.getStore()?.color;
}
__name(getTeammateColor, "getTeammateColor");
function runWithTeammateIdentity(identity, fn) {
  return teammateIdentityStore.run(identity, fn);
}
__name(runWithTeammateIdentity, "runWithTeammateIdentity");

// packages/core/src/agents/team/types.ts
init_esbuild_shims();
var MAX_TEAMMATES = 10;
var LEADER_NAME = "leader";
var TEAMS_DIR = "teams";
var TASKS_DIR = "tasks";
var TEAM_CONFIG_FILENAME = "config.json";
var INBOXES_DIR = "inboxes";
var TEAMMATE_COLORS = [
  "#FF6B6B",
  // red
  "#4ECDC4",
  // teal
  "#45B7D1",
  // blue
  "#FFA07A",
  // salmon
  "#98D8C8",
  // mint
  "#DDA0DD",
  // plum
  "#F0E68C",
  // khaki
  "#87CEEB",
  // sky blue
  "#FFB347",
  // orange
  "#B0E0E6"
  // powder blue
];

export {
  teammateIdentityStore,
  getTeammateContext,
  isInProcessTeammate,
  getAgentName,
  getTeamName,
  resolveActiveTeamName,
  isTeammate,
  isTeamLead,
  getTeammateColor,
  runWithTeammateIdentity,
  MAX_TEAMMATES,
  LEADER_NAME,
  TEAMS_DIR,
  TASKS_DIR,
  TEAM_CONFIG_FILENAME,
  INBOXES_DIR,
  TEAMMATE_COLORS
};
/**
 * @license
 * Copyright 2025 Qwen
 * SPDX-License-Identifier: Apache-2.0
 */
