#!/usr/bin/env node
import { GalAgent, GalAgentError } from './agent-client.mjs';

const usage = `Usage:
  node tools/agent.mjs context [--after CURSOR] [--limit N] [--text-units N]
  node tools/agent.mjs reply --request-id ID [--parent BLIP_ID] < reply.txt

Set GAL_AGENT_TOKEN and optionally GAL_URL (default http://127.0.0.1:8080).
Reply text is read from stdin. Keep the request id and text for retries.`;

try {
  const [command, ...args] = process.argv.slice(2);
  if (command === '--help' || command === 'help') {
    console.log(usage);
  } else {
    if (!['context', 'reply'].includes(command)) throw new Error(usage);
    const options = {};
    const allowed = command === 'context' ? ['--after', '--limit', '--text-units'] : ['--request-id', '--parent'];
    for (let i = 0; i < args.length; i += 2) {
      const flag = args[i];
      if (!allowed.includes(flag) || !args[i + 1] || args[i + 1].startsWith('--') || flag in options) throw new Error(`Invalid argument: ${flag}\n${usage}`);
      options[flag] = args[i + 1];
    }
    const client = new GalAgent({ baseUrl: process.env.GAL_URL, token: process.env.GAL_AGENT_TOKEN });
    let result;
    if (command === 'context') {
      const limit = options['--limit'] === undefined ? 50 : Number(options['--limit']);
      const textUnits = options['--text-units'] === undefined ? 16000 : Number(options['--text-units']);
      if (!Number.isInteger(limit) || limit < 1 || limit > 100 || !Number.isInteger(textUnits) || textUnits < 2 || textUnits > 64000) throw new Error('Invalid context bounds.');
      result = await client.context({ after: options['--after'], limit, textUnits });
    } else {
      if (!options['--request-id']) throw new Error(`reply requires --request-id.\n${usage}`);
      process.stdin.setEncoding('utf8');
      let text = '';
      for await (const chunk of process.stdin) {
        text += chunk;
        if (text.length > 262144) throw new Error('Reply exceeds 262144 UTF-16 units.');
      }
      result = await client.reply({ text, parent: options['--parent'], requestId: options['--request-id'] });
    }
    console.log(JSON.stringify(result, null, 2));
  }
} catch (error) {
  const message = error instanceof GalAgentError ? JSON.stringify({ error: error.message, status: error.status, code: error.code, requestId: error.requestId, retryAfterMs: error.retryAfterMs }) : error.message;
  console.error(message);
  process.exitCode = 1;
}
