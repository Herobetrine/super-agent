'use strict';
const { contextBridge, ipcRenderer } = require('electron');
const allowed = new Set([
  'chooseProject',
  'history',
  'snapshot',
  'windowMenu',
  'activityPage',
  'jobsPage',
  'defaults',
  'saveDefaults',
  'saveRoutes',
  'catalog',
  'catalogRevision',
  'create',
  'follow',
  'cancel',
  'retry',
  'note',
  'decide',
  'files',
  'diff',
  'planAdd',
  'planToggle',
  'export',
  'download',
  'extensions',
  'extensionDefaults',
  'mcpSave',
  'mcpRemove',
  'mcpProbe',
  'importSkill',
  'forgetSkill',
  'skillPreview'
]);
contextBridge.exposeInMainWorld('studio', {
  invoke(method, params = {}) {
    if (!allowed.has(method)) return Promise.reject(new Error('未知工作台操作'));
    return ipcRenderer.invoke('studio', method, params);
  }
});
