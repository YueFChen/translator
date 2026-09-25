/**
 * 插件界面文案资源表。
 *
 * 键 = `<域>.<语义>`，点分、段内 camelCase；域取页面 / 视图名：
 * `page`（页面骨架）、`mapping`（列映射）、`glossary`（词库）、`memory`（外部历史与翻译记忆）、
 * `preflight`（预检）、`export`（导出）、`provider`（模型服务设置）、`job`（任务运行）、
 * `common`（跨视图复用）。
 * 目标语言下拉的显示名见 `locales.ts`。
 *
 * 带参数的条目用 `{name}` 占位符，调用处传 `t(key, { name })`。
 */
export const zh = {
  'workspace.history': '工作记录',
  'workspace.collapseHistory': '收起工作记录',
  'workspace.expandHistory': '展开工作记录',
  'workspace.legacyWork': '历史导入 · {hash}',
  'workspace.workName': '工作名称',
  'workspace.renameWork': '重命名工作记录',
  'workspace.noTasks': '尚无翻译任务',
  'workspace.jobNumber': '翻译任务 #{id}',
  'workspace.toggle': '展开或收起',
  'workspace.selectWork': '选择左侧 CSV 工作记录，或导入新的 CSV。',
  'workspace.apiManage': 'API 管理',
  'workspace.parameters': '任务参数',
  'workspace.table': 'CSV 表格',
  'workspace.tableViews': '数据视图',
  'workspace.csvTab': 'CSV 数据',
  'workspace.search': '搜索当前表格',
  'workspace.actions': '操作',
  'workspace.rows': '行',
  'workspace.previous': '上一页',
  'workspace.next': '下一页',
  'workspace.row': '记录',
  'workspace.exportApproved': '正式导出',
  'workspace.exportLatestDraft': '导出所选任务草稿',
  'workspace.active': '当前使用',
  'workspace.activate': '使用',
  'workspace.newApi': '新增 API',
  'workspace.editApi': '编辑 API',
  'workspace.apiName': '模型显示名称',
  'workspace.deleteApi': '删除',
  /** 页面骨架。 */
  'page.title': '多语言翻译',
  'page.loading': '正在读取插件状态…',
  'page.chooseFile': '选择 CSV',

  /** 跨视图复用。 */
  'common.cancel': '取消操作',
  'common.save': '保存',
  'common.unknownError': '未知错误',
  'common.enabled': '启用',
  'common.disabled': '禁用',
  'common.edit': '编辑',
  'common.disable': '禁用',

  /** 列映射。 */

  /** 手工词库。 */
  'glossary.title': '项目词库',
  'glossary.kind': '类型',
  'glossary.kindOrdinary': '普通术语',
  'glossary.kindDoNotTranslate': '不可译词',
  'glossary.kindForbidden': '禁用译法',
  'glossary.sourceLocale': '源语言',
  'glossary.targetLocale': '目标语言',
  'glossary.sourceText': '源词',
  'glossary.targetText': '译名／禁用译法',
  'glossary.aliases': '别名（每行一个）',
  'glossary.context': '上下文（留空为通用）',
  'glossary.resourceKey': '资源键（留空为通用）',
  'glossary.disambiguation': '消歧说明',
  'glossary.create': '新增词条',
  'glossary.save': '保存修改',

  /** 翻译记忆。词条与记忆都不设人工审批，表格只读，唯一的操作是"禁用"。 */
  'memory.title': '翻译记忆',
  'memory.availability': '可用性',
  'memory.qaBlocked': '格式不符',

  /** 预检与 QA。 */
  'preflight.pending': '待填单元格',

  /** 导出。 */
  'export.done': '导出完成',

  /** 模型服务设置。 */
  'provider.baseUrl': '基础地址',
  'provider.apiFormat': 'API 格式',
  'provider.urlHint': '填写兼容 OpenAI 的基础地址；系统会自动添加 /chat/completions。',
  'provider.nameHint': '显示在工作区和 API 卡片中，可与调用用的模型 ID 不同。',
  'provider.remark': '备注',
  'provider.noRemark': '暂无备注',
  'provider.showKey': '显示密钥',
  'provider.hideKey': '隐藏密钥',
  'provider.advanced': '高级设置',
  'provider.model': '模型 ID（请求使用）',
  'provider.apiKey': 'API Key（留空保持现状）',
  'provider.allowLoopback': '允许本机回环端点',
  'provider.timeout': '超时秒数',
  'provider.rateLimit': '速率上限（次/分钟，留空为不限制）',
  'provider.priceInput': '每百万输入 token 单价',
  'provider.priceOutput': '每百万输出 token 单价',
  'provider.currency': '货币单位',
  'provider.save': '保存配置',
  'provider.clearKey': '清除密钥',

  /** 运行与进度。 */
  'job.concurrency': '并发数',
  'job.batchSize': '每批片段数',
  'job.batchHint': '一次请求打包多个片段；越大越省请求，也越容易被单条坏响应牵连。',
  'job.progress': '任务进度',
  'job.progressCount': '完成 {done} · 失败 {failed} · 共 {total}',
  'job.log': '运行日志',
  'job.logHint': '每行记录一个片段：模型返回的译文或失败原因。',
  'job.logEmpty': '暂无日志。',
  'job.usage': '本任务用量',
  'job.requests': '请求数',
  'job.inputTokens': '输入 token',
  'job.outputTokens': '输出 token',
  'job.cost': '估算费用',
  'job.inputBudget': '输入 token 预算',
  'job.includeFalseRows': '同时翻译 CSV 中标记为 FALSE 的行',
  'job.outputBudget': '输出 token 预算',
  'job.start': '开始翻译',
  'job.pause': '暂停',
  'job.resume': '继续',
  'job.cancel': '取消作业',
  'job.status': '状态',
  'job.statusQueued': '排队中',
  'job.statusRunning': '运行中',
  'job.statusPaused': '已暂停',
  'job.statusSucceeded': '已完成',
  'job.statusFailed': '失败',
  'job.statusCancelled': '已取消',
} as const
