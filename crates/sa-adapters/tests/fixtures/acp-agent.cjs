// Real stdio ACP v1 peer. No model/network use; used by Rust integration tests.
const readline = require('node:readline');
const fs = require('node:fs');
const mode = process.argv[2] || 'normal';
const receipt = process.argv[3];
const send = value => process.stdout.write(JSON.stringify({jsonrpc:'2.0',...value})+'\n');
let promptId, session='fixture-session';
const note = (kind, extra={}) => send({method:'session/update',params:{sessionId:mode==='wrong-session'?'wrong':session,update:{sessionUpdate:kind,...extra}}});
const finish=()=>{note('agent_message_chunk',{content:{type:'text',text:'精确结果\n'}});send({id:promptId,result:{stopReason:mode==='incomplete'?'max_tokens':'end_turn'}});};
readline.createInterface({input:process.stdin}).on('line',line=>{
 const message=JSON.parse(line);
 if(receipt)fs.appendFileSync(receipt,JSON.stringify(message)+'\n');
 const {id,method,params}=message;
 if(method==='initialize')return send({id,result:{protocolVersion:1,agentCapabilities:{loadSession:mode!=='no-load'},authMethods:[]}});
 if(method==='session/new'||method==='session/load'){
   if(method==='session/load'){session=params.sessionId;note('agent_message_chunk',{content:{type:'text',text:'OLD HISTORY'}});}
   return send({id,result:{...(method==='session/new'?{sessionId:session}:{}),modes:{currentModeId:'safe',availableModes:[{id:'safe',name:'Safe'}]},configOptions:[{id:'model',name:'Model',category:'model',type:'select',currentValue:'m1',options:[{value:'m1',name:'Model 1'}]}]}});
 }
 if(method==='session/set_mode'||method==='session/set_config_option')return send({id,result:method.endsWith('config_option')?{configOptions:[]}:{}});
 if(method==='session/prompt'){
   promptId=id;
   note('tool_call',{toolCallId:'tool-1',title:'Fixture write',kind:'edit',status:'pending'});
   if(mode==='wait')return;
   return send({id:99,method:'session/request_permission',params:{sessionId:session,toolCall:{toolCallId:'tool-1',title:'Fixture write'},options:[{optionId:'deny',name:'Reject',kind:'reject_once'},{optionId:'once',name:'Allow',kind:'allow_once'},{optionId:'always',name:'Always',kind:'allow_always'}]}});
 }
 if(id===99){note('tool_call_update',{toolCallId:'tool-1',status:message.result.outcome.optionId==='once'?'completed':'failed'});return finish();}
 if(method==='session/cancel'){send({id:promptId,result:{stopReason:'cancelled'}});return;}
 if(id!==undefined)send({id,error:{code:-32601,message:'not implemented'}});
});
