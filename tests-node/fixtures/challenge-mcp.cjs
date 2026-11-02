'use strict';
// Disposable local server for verifying real tool discovery, approval and calls.
const { Server } = require('@modelcontextprotocol/sdk/server/index.js');
const { StdioServerTransport } = require('@modelcontextprotocol/sdk/server/stdio.js');
const { StreamableHTTPServerTransport } = require('@modelcontextprotocol/sdk/server/streamableHttp.js');
const { ListToolsRequestSchema, CallToolRequestSchema } = require('@modelcontextprotocol/sdk/types.js');
const fs = require('node:fs');
function server() {
  const app = new Server({ name: 'studio-proof', version: '1' }, { capabilities: { tools: {} } });
  app.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [
      {
        name: 'challenge',
        description: 'Read the one-time verification code from the test service.',
        inputSchema: { type: 'object', properties: {}, additionalProperties: false }
      }
    ]
  }));
  app.setRequestHandler(CallToolRequestSchema, async (req) => {
    if (req.params.name !== 'challenge') throw Error('unknown tool');
    fs.appendFileSync(process.argv[2] + '.calls', '1\n');
    return { content: [{ type: 'text', text: fs.readFileSync(process.argv[2], 'utf8') }] };
  });
  return app;
}
if (process.argv[3] === 'http') {
  const http = require('node:http');
  const listener = http.createServer(async (req, res) => {
    if (req.url !== '/mcp') {
      res.writeHead(404).end();
      return;
    }
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: undefined,
      enableJsonResponse: true
    });
    const app = server();
    res.on('close', () => {
      void app.close();
    });
    await app.connect(transport);
    await transport.handleRequest(req, res);
  });
  listener.listen(0, '127.0.0.1', () => process.send({ port: listener.address().port }));
} else
  server()
    .connect(new StdioServerTransport())
    .catch(() => process.exit(1));
