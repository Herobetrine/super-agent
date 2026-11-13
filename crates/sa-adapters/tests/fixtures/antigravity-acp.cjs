// Isolated native-image ACP peer. It never calls a model or the network.
const fs = require('node:fs');
const readline = require('node:readline');
const [scenario, receipt] = process.argv.slice(2);
const send = value => process.stdout.write(JSON.stringify({jsonrpc: '2.0', ...value}) + '\n');
let session = 'agy-fixture-session';
let model = 'gemini-fixture-high';
let effort = 'high';
let mode = 'default';
let promptId;
const options = () => [
  {id: 'thought_level', name: 'Thinking', type: 'select', currentValue: effort,
    options: ['low', 'high'].map(value => ({value, name: value}))},
];
const state = () => ({sessionId: session,
  _meta: scenario === 'wrong-source' ? {} : {antigravity: {tier: {id: 'fixture-google-tier'}}},
  models: {currentModelId: scenario === 'wrong-model' ? 'other-model' : model,
    availableModels: ['gemini-fixture-high', 'gemini-fixture-low'].map(modelId => ({modelId, name: modelId}))},
  modes: {currentModeId: mode, availableModes: [{id: 'default', name: 'Default'}]},
  configOptions: options()});
const note = (kind, extra) => send({method: 'session/update', params: {
  sessionId: scenario === 'wrong-session' ? 'foreign-session' : session,
  update: {sessionUpdate: kind, ...extra},
}});
readline.createInterface({input: process.stdin}).on('line', line => {
  const message = JSON.parse(line);
  fs.appendFileSync(receipt, JSON.stringify(message) + '\n');
  const {id, method, params} = message;
  if (method === 'initialize') return send({id, result: {protocolVersion: 1,
    agentCapabilities: {loadSession: scenario !== 'no-load', promptCapabilities: {image: scenario !== 'no-image'}}, authMethods: []}});
  if (method === 'session/new' || method === 'session/load') {
    if (method === 'session/load') {
      if (scenario !== 'wrong-load') session = params.sessionId;
      else session = 'wrong-loaded-session';
      note('agent_message_chunk', {content: {type: 'text', text: 'OLD HISTORY'}});
    }
    return send({id, result: state()});
  }
  if (method === 'session/set_model') {model = params.modelId; return send({id, result: {}});}
  if (method === 'session/set_mode') {mode = params.modeId; return send({id, result: {}});}
  if (method === 'session/set_config_option') {
    if (scenario !== 'wrong-effort') effort = params.value;
    return send({id, result: {configOptions: options()}});
  }
  if (method === 'session/prompt') {
    promptId = id;
    if (scenario === 'wait') return;
    note('agent_message_chunk', {content: {type: 'text', text: '中途内容\n'}});
    if (scenario === 'overloaded') return send({id, error: {code: -32603,
      message: 'API error (attempt 4): UNAVAILABLE (code 503): No capacity available for model secret-model'}});
    if (scenario === 'incomplete') return send({id, result: {stopReason: 'max_tokens'}});
    return send({id: 99, method: 'session/request_permission', params: {sessionId: session,
      toolCall: {toolCallId: 'fixture-write', title: 'Write fixture'},
      options: [{optionId: 'deny', name: 'Deny', kind: 'reject_once'}, {optionId: 'allow', name: 'Allow', kind: 'allow_once'}]}});
  }
  if (id === 99) {
    note('agent_message_chunk', {content: {type: 'text', text: '图片完成\n'}});
    return send({id: promptId, result: {stopReason: 'end_turn'}});
  }
  if (method === 'session/cancel') return send({id: promptId, result: {stopReason: 'cancelled'}});
  if (id !== undefined) send({id, error: {code: -32601, message: 'Unsupported method'}});
});
