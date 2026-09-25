import { useEffect, useRef, useState } from 'react'
import type { CsvPage, GlossaryTerm, JobLimits, JobProgress, MemoryEntry, Preflight, ProviderConfigInput, ProviderProfile, TermInput, TranslationExport, WorkRecord, WorkTree } from './types.generated'
import type { TranslatorApi } from './index'
import { t } from './i18n'

type Drawer = 'providers' | 'parameters' | 'termEditor' | null
type TableView = 'csv' | 'terms' | 'memory'
const PAGE_SIZE = 50
const LOG_LIMIT = 1000
const EMPTY_TREE: WorkTree = { records: [], jobs: [] }
const DEFAULT_LIMITS: JobLimits = { concurrency: 8, batch_size: 16, include_false_rows: false, input_token_budget: 0, output_token_budget: 0 }
const EMPTY_PROVIDER: ProviderConfigInput = { base_url: '', model: '', remark: '', allow_loopback: false, timeout_seconds: 120, requests_per_minute: null, price_per_million_input_tokens: null, price_per_million_output_tokens: null, currency: null, api_key: null }
const EMPTY_TERM: TermInput = { id: null, kind: 'ordinary', source_locale: { tag: 'zh-Hans' }, target_locale: { tag: 'en' }, source_text: '', target_text: '', aliases: [], context: null, resource_key: null, disambiguation: '', expected_version: null }

function message(cause: unknown): string {
  if (typeof cause === 'string') return cause
  if (cause instanceof Error) return cause.message
  return t('common.unknownError')
}

function statusLabel(progress: JobProgress): string {
  return t(`job.status${progress.status[0].toUpperCase()}${progress.status.slice(1)}` as 'job.statusSucceeded')
}

function profileInput(profile: ProviderProfile): ProviderConfigInput {
  const { has_key: _hasKey, ...config } = profile.config
  return { ...config, api_key: null }
}

function workTitle(record: WorkRecord): string {
  if (record.title) return record.title
  return isSnapshotName(record.file_name)
    ? t('workspace.legacyWork', { hash: record.input_hash.slice(0, 8) })
    : record.file_name.replace(/\.csv$/i, '')
}

function isSnapshotName(name: string): boolean {
  return /^[0-9a-f]{64}\.csv$/i.test(name)
}

export function TranslatorPage({ api }: { api: TranslatorApi }) {
  const [tree, setTree] = useState<WorkTree>(EMPTY_TREE)
  const [selectedWork, setSelectedWork] = useState<number | null>(null)
  const [selectedJob, setSelectedJob] = useState<number | null>(null)
  const [preflight, setPreflight] = useState<Preflight | null>(null)
  const [page, setPage] = useState<CsvPage | null>(null)
  const [offset, setOffset] = useState(0)
  const [tableView, setTableView] = useState<TableView>('csv')
  const [resourceOffset, setResourceOffset] = useState(0)
  const [resourceSearch, setResourceSearch] = useState('')
  const [drawer, setDrawer] = useState<Drawer>(null)
  const [historyCollapsed, setHistoryCollapsed] = useState(() => {
    try { return localStorage.getItem('wonderland.translator.historyCollapsed') === 'true' } catch { return false }
  })
  const [editingWorkName, setEditingWorkName] = useState(false)
  const [workName, setWorkName] = useState('')
  const [apiModal, setApiModal] = useState(false)
  const [showApiKey, setShowApiKey] = useState(false)
  const apiDialog = useRef<HTMLDialogElement>(null)
  const [profiles, setProfiles] = useState<ProviderProfile[]>([])
  const [editingProfile, setEditingProfile] = useState<number | null>(null)
  const [profileName, setProfileName] = useState('')
  const [profileForm, setProfileForm] = useState<ProviderConfigInput>(EMPTY_PROVIDER)
  const [apiKey, setApiKey] = useState('')
  const [limits, setLimits] = useState<JobLimits>(DEFAULT_LIMITS)
  const [progress, setProgress] = useState<JobProgress | null>(null)
  const [result, setResult] = useState<TranslationExport | null>(null)
  const [busy, setBusy] = useState(false)
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Record<number, boolean>>({})
  const [terms, setTerms] = useState<GlossaryTerm[]>([])
  const [memory, setMemory] = useState<MemoryEntry[]>([])
  const [term, setTerm] = useState<TermInput>(EMPTY_TERM)
  const [logs, setLogs] = useState<string[]>([])
  const [logsOpen, setLogsOpen] = useState(true)
  const logRef = useRef<HTMLOListElement>(null)
  /** 最近一次作业的 id：出错时要拿它把库里的真实状态读回来。 */
  const jobIdRef = useRef<number | null>(null)
  const currentRecord = tree.records.find(record => record.id === selectedWork) ?? null
  const currentProfile = profiles.find(profile => profile.active) ?? null
  const selectedJobRecord = tree.jobs.find(job => job.id === selectedJob) ?? null
  // 只有已完成的作业没什么可续的；取消、失败、暂停、遗留的运行中/排队中都还能接着跑。
  const canResume = selectedJob !== null && progress !== null && progress.status !== 'succeeded'

  const appendLog = (lines: string[]) => setLogs(previous => [...previous, ...lines].slice(-LOG_LIMIT))
  // 暂停/取消只在真有一个运行中的作业时才可点：progress 的状态来自库里，而库里的 running
  // 现在不会骗人（异常退出的作业会被收口成失败或已暂停）。
  const jobRunning = running || progress?.status === 'running'

  useEffect(() => {
    const dialog = apiDialog.current
    if (!dialog) return
    if (apiModal && !dialog.open) dialog.showModal()
    if (!apiModal && dialog.open) dialog.close()
  }, [apiModal])

  useEffect(() => {
    const list = logRef.current
    if (list) list.scrollTop = list.scrollHeight
  }, [logs, logsOpen])

  const toggleHistory = () => setHistoryCollapsed(value => {
    try { localStorage.setItem('wonderland.translator.historyCollapsed', String(!value)) } catch { /* 仅影响布局记忆。 */ }
    return !value
  })

  const refreshTree = async () => setTree(await api.workTree())
  const refreshProfiles = async () => setProfiles(await api.providerProfiles())
  const refreshResources = async () => {
    const [nextTerms, nextMemory] = await Promise.all([api.glossaryTerms(), api.memoryEntries()])
    setTerms(nextTerms); setMemory(nextMemory)
  }

  useEffect(() => {
    let active = true
    Promise.all([api.workTree(), api.providerProfiles(), api.glossaryTerms(), api.memoryEntries()]).then(([nextTree, nextProfiles, nextTerms, nextMemory]) => {
      if (active) { setTree(nextTree); setProfiles(nextProfiles); setTerms(nextTerms); setMemory(nextMemory) }
    }).catch(cause => { if (active) setError(message(cause)) })
    return () => { active = false }
  }, [api])

  useEffect(() => {
    if (selectedWork === null) { setPage(null); return }
    let active = true
    setPage(null)
    api.csvPage(selectedWork, selectedJob, offset, PAGE_SIZE).then(value => {
      if (active) setPage(value)
    }).catch(cause => { if (active) setError(message(cause)) })
    return () => { active = false }
  }, [api, selectedWork, selectedJob, offset, progress?.done])

  const act = async (action: () => Promise<void>) => {
    setBusy(true); setError(null)
    try { await action() } catch (cause) { const detail = message(cause); setError(detail); appendLog([detail]) }
    finally { setBusy(false) }
  }

  const choose = () => void act(async () => {
    const file = await api.chooseFile()
    if (!file) return
    const next = await api.prepare(file.default_mapping)
    setPreflight(next); setSelectedJob(null); setProgress(null); setOffset(0)
    setTableView('csv')
    const nextTree = await api.workTree()
    setTree(nextTree)
    setSelectedWork(nextTree.records[0]?.id ?? null)
  })

  const openRecord = (record: WorkRecord, jobId: number | null = null) => void act(async () => {
    if (running) return
    setLogs([])
    const next = await api.openWork(record.id)
    setPreflight(next); setSelectedWork(record.id)
    setEditingWorkName(false)
    setSelectedJob(jobId); setOffset(0); setResult(null)
    jobIdRef.current = jobId
    setProgress(jobId === null ? null : await api.workJobProgress(jobId))
  })

  const runJob = (resume: boolean) => {
    setRunning(true); setError(null); setResult(null)
    setLogs([])
    const update = (next: JobProgress) => {
      jobIdRef.current = next.job_id
      setProgress(next); setSelectedJob(next.job_id)
      // 终态一到就解锁界面：即使这条命令的 promise 因为别的原因迟迟不落地，
      // 按钮也不会被一个过期的 running 标志锁死。
      if (next.status !== 'running') setRunning(false)
    }
    appendLog([resume ? t('job.resume') : t('job.start')])
    void (resume && selectedJob !== null ? api.jobResumeSelected(selectedJob, limits, update, appendLog) : api.jobStart(limits, update, appendLog)).then(async next => {
      update(next); await refreshTree()
    }).catch(async cause => {
      const detail = message(cause)
      setError(detail); appendLog([detail])
      // 作业异常结束时把库里的真实状态读回来：否则界面会停在最后一条"运行中"的进度上，
      // 让用户以为还能暂停（后端其实已经没有在跑的作业了）。
      const jobId = jobIdRef.current
      if (jobId !== null) setProgress(await api.workJobProgress(jobId).catch(() => null))
      await refreshTree().catch(() => undefined)
    }).finally(() => setRunning(false))
  }

  const controlJob = (kind: 'pause' | 'cancel') => {
    appendLog([kind === 'pause' ? t('job.pause') : t('job.cancel')])
    void (kind === 'pause' ? api.jobPause() : api.jobCancel()).catch(cause => { const detail = message(cause); setError(detail); appendLog([detail]) })
  }

  const exportFile = (drafts: boolean) => void act(async () => {
    if (drafts && selectedJob === null) return
    const exported = await (drafts ? api.exportWorkDrafts(selectedJob!) : api.export())
    setResult(exported)
    appendLog([`${t('export.done')} · ${exported.csv_path}`])
  })

  const selectProfile = (profile: ProviderProfile) => {
    setEditingProfile(profile.id); setProfileName(profile.name); setProfileForm(profileInput(profile)); setApiKey(''); setShowApiKey(false); setApiModal(true)
  }

  const saveProfile = () => void act(async () => {
    const profile = await api.saveProfile(editingProfile, profileName.trim(), { ...profileForm, api_key: apiKey === '' ? null : apiKey })
    setEditingProfile(profile.id); setApiKey(''); await refreshProfiles(); setApiModal(false)
  })

  const saveWorkName = () => void act(async () => {
    if (selectedWork === null || !workName.trim()) return
    await api.renameWork(selectedWork, workName.trim())
    await refreshTree()
    setEditingWorkName(false)
  })

  const changeView = (view: TableView) => {
    setTableView(view); setResourceOffset(0); setResourceSearch('')
  }

  const renderRecord = (record: WorkRecord) => {
    const open = expanded[record.id] ?? true
    const jobs = tree.jobs.filter(job => job.work_id === record.id)
    return <div className="tr-tree-record" key={record.id}>
      <div className="tr-tree-line">
        <button type="button" className="tr-tree-toggle" aria-label={t('workspace.toggle')} onClick={() => setExpanded({ ...expanded, [record.id]: !open })}>{open ? '▾' : '▸'}</button>
        <button type="button" className="tr-tree-item" data-active={selectedWork === record.id && selectedJob === null} onClick={() => openRecord(record)} title={workTitle(record)}>{workTitle(record)}</button>
      </div>
      {open && <div className="tr-tree-children">
        {jobs.length === 0 && <span className="tr-tree-empty">{t('workspace.noTasks')}</span>}
        {jobs.map(job => <button type="button" key={job.id} className="tr-tree-item tr-job-node" data-active={selectedJob === job.id} onClick={() => openRecord(record, job.id)}>
          {t('workspace.jobNumber', { id: job.id })}<span>{t(`job.status${job.status[0].toUpperCase()}${job.status.slice(1)}` as 'job.statusSucceeded')}</span>
        </button>)}
      </div>}
    </div>
  }

  const query = resourceSearch.trim().toLocaleLowerCase()
  const visibleTerms = query
    ? terms.filter(item => [item.source_text, item.target_text, item.source_locale.tag, item.target_locale.tag].some(value => value.toLocaleLowerCase().includes(query)))
    : terms
  const visibleMemory = query
    ? memory.filter(item => [item.source_text, item.target_text, item.source_locale.tag, item.target_locale.tag].some(value => value.toLocaleLowerCase().includes(query)))
    : memory
  const resourceTotal = tableView === 'terms' ? visibleTerms.length : visibleMemory.length
  const pageStart = tableView === 'csv' ? offset : resourceOffset
  const pageCount = tableView === 'csv' ? page?.rows.length ?? 0 : Math.max(0, Math.min(PAGE_SIZE, resourceTotal - resourceOffset))
  const canNext = tableView === 'csv' ? !!page?.has_more : resourceOffset + PAGE_SIZE < resourceTotal
  const changePage = (next: number) => tableView === 'csv' ? setOffset(next) : setResourceOffset(next)
  const finished = progress ? progress.done + progress.failed : 0
  const percent = progress ? Math.round(finished / Math.max(progress.total, 1) * 100) : 0

  return <div className="tr-workspace" data-history-collapsed={historyCollapsed}>
    <aside className="tr-worktree" aria-label={t('workspace.history')}>
      <header className="tr-worktree-head">
        <button type="button" className="tr-icon-button" onClick={toggleHistory} aria-label={historyCollapsed ? t('workspace.expandHistory') : t('workspace.collapseHistory')} title={historyCollapsed ? t('workspace.expandHistory') : t('workspace.collapseHistory')}>{historyCollapsed ? '›' : '‹'}</button>
        {!historyCollapsed && <h2>{t('workspace.history')}</h2>}
        <button className="tr-button tr-primary tr-import-button" disabled={busy || running} onClick={choose} title={t('page.chooseFile')}>{historyCollapsed ? '+' : t('page.chooseFile')}</button>
      </header>
      {!historyCollapsed && <nav className="tr-tree">{tree.records.map(renderRecord)}</nav>}
    </aside>

    <main className="tr-workmain">
      <section className="tr-toolbar">
        <div className="tr-toolbar-title"><div className="tr-title-line">{editingWorkName && currentRecord ? <form className="tr-title-edit" onSubmit={event => { event.preventDefault(); saveWorkName() }}><input className="tr-select" value={workName} maxLength={120} autoFocus onChange={event => setWorkName(event.target.value)} aria-label={t('workspace.workName')} /><button className="tr-button tr-primary" disabled={busy || !workName.trim()}>{t('common.save')}</button><button type="button" className="tr-button" onClick={() => setEditingWorkName(false)}>{t('common.cancel')}</button></form> : <><h1 title={currentRecord?.file_name}>{selectedJobRecord ? t('workspace.jobNumber', { id: selectedJobRecord.id }) : currentRecord ? workTitle(currentRecord) : t('page.title')}</h1>{currentRecord && <button type="button" className="tr-icon-button" title={t('workspace.renameWork')} aria-label={t('workspace.renameWork')} onClick={() => { setWorkName(workTitle(currentRecord)); setEditingWorkName(true) }}>✎</button>}</>}</div></div>
        <div className="tr-toolbar-actions">
          <button className="tr-button" onClick={() => setDrawer('providers')}>{currentProfile?.name ?? t('workspace.apiManage')}</button>
          <button className="tr-button" onClick={() => setDrawer('parameters')}>{t('workspace.parameters')}</button>
          <span className="tr-toolbar-divider" />
          <button className="tr-button tr-primary" disabled={busy || running || !preflight} onClick={() => runJob(false)}>{t('job.start')}</button>
          <button className="tr-button" disabled={busy || running || !canResume} onClick={() => runJob(true)}>{t('job.resume')}</button>
          <button className="tr-button" disabled={!jobRunning} onClick={() => controlJob('pause')}>{t('job.pause')}</button>
          <button className="tr-button" disabled={!jobRunning} onClick={() => controlJob('cancel')}>{t('job.cancel')}</button>
          <span className="tr-toolbar-divider" />
          <button className="tr-button" disabled={busy || running || !preflight} onClick={() => exportFile(false)}>{t('workspace.exportApproved')}</button>
          <button className="tr-button" disabled={busy || running || !preflight || selectedJob === null} onClick={() => exportFile(true)}>{t('workspace.exportLatestDraft')}</button>
        </div>
      </section>

      {(progress || logs.length > 0) && <section className="tr-run-panel" aria-label={t('job.progress')}>
        <div className="tr-progress-head">
          {progress ? <>
            <div className="tr-progress-bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent} aria-label={t('job.progress')}><span data-state={progress.failed > 0 ? 'failed' : 'ok'} style={{ width: `${percent}%` }} /></div>
            <strong>{percent}%</strong>
            <span className="tr-progress-count">{t('job.progressCount', { done: progress.done, failed: progress.failed, total: progress.total })}</span>
            <span className="tr-progress-state">{statusLabel(progress)}{progress.note ? ` · ${progress.note}` : ''}</span>
          </> : <span className="tr-progress-state">{t('job.progress')}</span>}
          <button type="button" className="tr-button" title={t('job.logHint')} onClick={() => setLogsOpen(value => !value)}>{t('job.log')}</button>
        </div>
        {logsOpen && <ol className="tr-log" ref={logRef}>{logs.length === 0 ? <li className="tr-muted">{t('job.logEmpty')}</li> : logs.map((line, index) => <li key={index}>{line}</li>)}</ol>}
      </section>}

      <section className="tr-table-panel" aria-label={t('workspace.table')}>
        <header className="tr-table-head">
          <nav className="tr-table-tabs" aria-label={t('workspace.tableViews')}>
            <button data-active={tableView === 'csv'} onClick={() => changeView('csv')}>{t('workspace.csvTab')}</button>
            <button data-active={tableView === 'terms'} onClick={() => changeView('terms')}>{t('glossary.title')}</button>
            <button data-active={tableView === 'memory'} onClick={() => changeView('memory')}>{t('memory.title')}</button>
          </nav>
          {tableView !== 'csv' && <div className="tr-table-tools"><input className="tr-select" value={resourceSearch} onChange={event => { setResourceSearch(event.target.value); setResourceOffset(0) }} placeholder={t('workspace.search')} aria-label={t('workspace.search')} />{tableView === 'terms' && <button className="tr-button tr-primary" onClick={() => { setTerm(EMPTY_TERM); setDrawer('termEditor') }}>{t('glossary.create')}</button>}</div>}
        </header>
        {tableView === 'csv' && (page ? <div className="tr-table-scroll"><table className="tr-data-table"><thead><tr><th>{t('workspace.row')}</th>{page.headers.map((header, index) => <th key={index}>{header}</th>)}</tr></thead><tbody>{page.rows.map((row, rowIndex) => <tr key={offset + rowIndex}><th>{offset + rowIndex + 2}</th>{row.map((cell, index) => <td key={index} title={cell}>{cell}</td>)}</tr>)}</tbody></table></div> : <div className="tr-table-placeholder">{selectedWork === null ? t('workspace.selectWork') : t('page.loading')}</div>)}
        {tableView === 'terms' && <div className="tr-table-scroll"><table className="tr-data-table tr-resource-table"><thead><tr><th>ID</th><th>{t('glossary.sourceText')}</th><th>{t('glossary.targetText')}</th><th>{t('glossary.sourceLocale')}</th><th>{t('glossary.targetLocale')}</th><th>{t('glossary.kind')}</th><th>{t('memory.availability')}</th><th>{t('workspace.actions')}</th></tr></thead><tbody>{visibleTerms.slice(resourceOffset, resourceOffset + PAGE_SIZE).map(item => <tr key={item.id}><th>{item.id}</th><td title={item.source_text}>{item.source_text}</td><td title={item.target_text}>{item.target_text}</td><td>{item.source_locale.tag}</td><td>{item.target_locale.tag}</td><td>{t(`glossary.kind${item.kind === 'do_not_translate' ? 'DoNotTranslate' : item.kind === 'forbidden' ? 'Forbidden' : 'Ordinary'}` as 'glossary.kindOrdinary')}</td><td>{item.enabled ? t('common.enabled') : t('common.disabled')}</td><td className="tr-cell-actions"><button onClick={() => { setTerm({ id: item.id, kind: item.kind, source_locale: item.source_locale, target_locale: item.target_locale, source_text: item.source_text, target_text: item.target_text, aliases: item.aliases, context: item.context, resource_key: item.resource_key, disambiguation: item.disambiguation, expected_version: item.version }); setDrawer('termEditor') }}>{t('common.edit')}</button>{item.enabled && <button onClick={() => void act(async () => { await api.disableTerm(item.id); await refreshResources(); if (selectedWork !== null) setPreflight(await api.openWork(selectedWork)) })}>{t('common.disable')}</button>}</td></tr>)}</tbody></table></div>}
        {tableView === 'memory' && <div className="tr-table-scroll"><table className="tr-data-table tr-resource-table"><thead><tr><th>ID</th><th>{t('glossary.sourceText')}</th><th>{t('glossary.targetText')}</th><th>{t('glossary.sourceLocale')}</th><th>{t('glossary.targetLocale')}</th><th>{t('memory.availability')}</th><th>{t('workspace.actions')}</th></tr></thead><tbody>{visibleMemory.slice(resourceOffset, resourceOffset + PAGE_SIZE).map(item => <tr key={item.id}><th>{item.id}</th><td title={item.source_text}>{item.source_text}</td><td title={item.target_text}>{item.target_text}</td><td>{item.source_locale.tag}</td><td>{item.target_locale.tag}</td><td>{item.qa_blocking ? t('memory.qaBlocked') : item.enabled ? t('common.enabled') : t('common.disabled')}</td><td className="tr-cell-actions">{item.enabled && <button onClick={() => void act(async () => { await api.disableMemory(item.id); await refreshResources(); if (selectedWork !== null) setPreflight(await api.openWork(selectedWork)) })}>{t('common.disable')}</button>}</td></tr>)}</tbody></table></div>}
        <footer className="tr-table-footer"><div className="tr-table-status">{error ? <span className="tr-status-error" role="alert">{error}</span> : result ? <span title={result.csv_path}>{t('export.done')} · {result.csv_path}</span> : progress ? <span>{statusLabel(progress)} · {finished}/{progress.total}</span> : tableView === 'csv' && preflight ? <span>{preflight.total_rows} {t('workspace.rows')}{!limits.include_false_rows && ` · ${preflight.pending_cells} ${t('preflight.pending')}`}</span> : null}</div><div className="tr-pagination"><button className="tr-button" disabled={pageStart === 0} onClick={() => changePage(Math.max(0, pageStart - PAGE_SIZE))}>{t('workspace.previous')}</button><span>{pageCount ? pageStart + 1 : 0}–{pageStart + pageCount}{tableView !== 'csv' ? ` / ${resourceTotal}` : ''}</span><button className="tr-button" disabled={!canNext} onClick={() => changePage(pageStart + PAGE_SIZE)}>{t('workspace.next')}</button></div></footer>
      </section>
    </main>

    {drawer === 'providers' && <div className="tr-drawer-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) setDrawer(null) }}><aside className="tr-drawer-panel" aria-label={t('workspace.apiManage')}><header className="tr-drawer-head"><h2>{t('workspace.apiManage')}</h2><button type="button" className="tr-icon-button" onClick={() => setDrawer(null)}>×</button></header><div className="tr-drawer tr-api-list">
      <div className="tr-profile-list">{profiles.map(profile => <article key={profile.id} className="tr-profile-card" data-active={profile.active}>
        <div className="tr-profile-model" title={profile.name}><strong>{profile.name}</strong><small title={profile.config.model}>{profile.config.model}</small></div>
        <div className="tr-profile-info"><strong title={profile.config.base_url}>{profile.config.base_url}</strong><small title={profile.config.remark || t('provider.noRemark')}>{profile.config.remark || t('provider.noRemark')}</small></div>
        <div className="tr-profile-actions">
          <button className="tr-button tr-primary" disabled={busy || running || profile.active} onClick={() => void act(async () => { await api.activateProfile(profile.id); await refreshProfiles() })}>{profile.active ? t('workspace.active') : t('workspace.activate')}</button>
          <button className="tr-button" disabled={busy || running} onClick={() => selectProfile(profile)}>{t('common.edit')}</button>
          <button className="tr-button" disabled={busy || running || profile.active} onClick={() => void act(async () => { await api.deleteProfile(profile.id); await refreshProfiles() })}>{t('workspace.deleteApi')}</button>
        </div>
      </article>)}</div>
      <button className="tr-button tr-primary tr-add-api" disabled={busy || running} onClick={() => { setEditingProfile(null); setProfileName(''); setProfileForm(EMPTY_PROVIDER); setApiKey(''); setShowApiKey(false); setApiModal(true) }}>＋ {t('workspace.newApi')}</button>
    </div></aside></div>}

    <dialog ref={apiDialog} className="tr-api-dialog" aria-labelledby="tr-api-dialog-title" onCancel={() => setApiModal(false)} onClose={() => setApiModal(false)}>
      <form onSubmit={event => { event.preventDefault(); saveProfile() }}>
        <header className="tr-dialog-head"><h2 id="tr-api-dialog-title">{editingProfile === null ? t('workspace.newApi') : t('workspace.editApi')}</h2><button type="button" className="tr-icon-button" aria-label={t('common.cancel')} onClick={() => setApiModal(false)}>×</button></header>
        <div className="tr-dialog-body">
          <label>{t('provider.apiFormat')}<input className="tr-select" value="OpenAI Chat Completions" readOnly /></label>
          <label>{t('provider.baseUrl')}<span className="tr-field-hint">{t('provider.urlHint')}</span><input className="tr-select" type="url" required placeholder="https://api.example.com/v1" value={profileForm.base_url} onChange={event => setProfileForm({ ...profileForm, base_url: event.target.value })} /></label>
          <label>{t('provider.model')}<input className="tr-select" required value={profileForm.model} onChange={event => setProfileForm({ ...profileForm, model: event.target.value })} /></label>
          <label>{t('workspace.apiName')}<span className="tr-field-hint">{t('provider.nameHint')}</span><input className="tr-select" required maxLength={80} value={profileName} onChange={event => setProfileName(event.target.value)} /></label>
          <label>{t('provider.remark')}<textarea className="tr-select" rows={2} maxLength={200} value={profileForm.remark} onChange={event => setProfileForm({ ...profileForm, remark: event.target.value })} /></label>
          <label>{t('provider.apiKey')}<span className="tr-key-field"><input className="tr-select" type={showApiKey ? 'text' : 'password'} required={editingProfile === null} value={apiKey} onChange={event => setApiKey(event.target.value)} autoComplete="off" /><button type="button" className="tr-icon-button" onClick={() => setShowApiKey(value => !value)} aria-label={showApiKey ? t('provider.hideKey') : t('provider.showKey')}>{showApiKey ? '◉' : '◎'}</button></span></label>
          <details className="tr-advanced"><summary>{t('provider.advanced')}</summary><div className="tr-advanced-fields">
            <label>{t('provider.timeout')}<input className="tr-select" type="number" min="10" max="600" value={profileForm.timeout_seconds} onChange={event => setProfileForm({ ...profileForm, timeout_seconds: Number(event.target.value) })} /></label>
            <label>{t('provider.rateLimit')}<input className="tr-select" type="number" min="0" value={profileForm.requests_per_minute ?? ''} onChange={event => setProfileForm({ ...profileForm, requests_per_minute: event.target.value === '' ? null : Number(event.target.value) })} /></label>
            <label>{t('provider.priceInput')}<input className="tr-select" type="number" min="0" step="any" value={profileForm.price_per_million_input_tokens ?? ''} onChange={event => setProfileForm({ ...profileForm, price_per_million_input_tokens: event.target.value === '' ? null : Number(event.target.value) })} /></label>
            <label>{t('provider.priceOutput')}<input className="tr-select" type="number" min="0" step="any" value={profileForm.price_per_million_output_tokens ?? ''} onChange={event => setProfileForm({ ...profileForm, price_per_million_output_tokens: event.target.value === '' ? null : Number(event.target.value) })} /></label>
            <label>{t('provider.currency')}<input className="tr-select" value={profileForm.currency ?? ''} onChange={event => setProfileForm({ ...profileForm, currency: event.target.value || null })} /></label>
            <label className="tr-check"><input type="checkbox" checked={profileForm.allow_loopback} onChange={event => setProfileForm({ ...profileForm, allow_loopback: event.target.checked })} />{t('provider.allowLoopback')}</label>
            {editingProfile !== null && <button type="button" className="tr-button" disabled={busy} onClick={() => void act(async () => { await api.saveProfile(editingProfile, profileName.trim(), { ...profileForm, api_key: '' }); setApiKey(''); await refreshProfiles() })}>{t('provider.clearKey')}</button>}
          </div></details>
          {error && <p className="tr-error" role="alert">{error}</p>}
        </div>
        <footer className="tr-dialog-footer"><button type="button" className="tr-button" onClick={() => setApiModal(false)}>{t('common.cancel')}</button><button className="tr-button tr-primary" disabled={busy || running || !profileName.trim() || !profileForm.base_url.trim() || !profileForm.model.trim() || (editingProfile === null && !apiKey.trim())}>{editingProfile === null ? t('workspace.newApi') : t('provider.save')}</button></footer>
      </form>
    </dialog>

    {drawer === 'parameters' && <div className="tr-drawer-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) setDrawer(null) }}><aside className="tr-drawer-panel" aria-label={t('workspace.parameters')}><header className="tr-drawer-head"><h2>{t('workspace.parameters')}</h2><button type="button" className="tr-icon-button" onClick={() => setDrawer(null)}>×</button></header><div className="tr-drawer">
      <label>{t('job.concurrency')}<input className="tr-select" type="number" min="1" max="32" value={limits.concurrency} onChange={event => setLimits({ ...limits, concurrency: Number(event.target.value) })} /></label>
      <label>{t('job.batchSize')}<span className="tr-field-hint">{t('job.batchHint')}</span><input className="tr-select" type="number" min="1" max="32" value={limits.batch_size} onChange={event => setLimits({ ...limits, batch_size: Number(event.target.value) })} /></label>
      <label className="tr-check"><input type="checkbox" checked={limits.include_false_rows} onChange={event => setLimits({ ...limits, include_false_rows: event.target.checked })} />{t('job.includeFalseRows')}</label>
      <label>{t('job.inputBudget')}<input className="tr-select" type="number" min="0" value={limits.input_token_budget} onChange={event => setLimits({ ...limits, input_token_budget: Number(event.target.value) })} /></label>
      <label>{t('job.outputBudget')}<input className="tr-select" type="number" min="0" value={limits.output_token_budget} onChange={event => setLimits({ ...limits, output_token_budget: Number(event.target.value) })} /></label>
      {selectedJob !== null && progress && <div className="tr-job-usage"><h3>{t('job.usage')}</h3><span>{t('job.requests')}: {progress.requests}</span><span>{t('job.inputTokens')}: {progress.input_tokens}</span><span>{t('job.outputTokens')}: {progress.output_tokens}</span>{progress.estimated_cost !== null && <strong>{t('job.cost')}: {progress.estimated_cost.toFixed(4)} {progress.currency ?? ''}</strong>}</div>}
    </div></aside></div>}

    {drawer === 'termEditor' && <div className="tr-drawer-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) setDrawer(null) }}><aside className="tr-drawer-panel" aria-label={term.id === null ? t('glossary.create') : t('glossary.save')}><header className="tr-drawer-head"><h2>{term.id === null ? t('glossary.create') : t('glossary.save')}</h2><button type="button" className="tr-icon-button" onClick={() => setDrawer(null)}>×</button></header><div className="tr-drawer">
      <label>{t('glossary.kind')}<select className="tr-select" value={term.kind} onChange={event => setTerm({ ...term, kind: event.target.value as TermInput['kind'] })}><option value="ordinary">{t('glossary.kindOrdinary')}</option><option value="do_not_translate">{t('glossary.kindDoNotTranslate')}</option><option value="forbidden">{t('glossary.kindForbidden')}</option></select></label>
      <label>{t('glossary.sourceLocale')}<input className="tr-select" value={term.source_locale.tag} onChange={event => setTerm({ ...term, source_locale: { tag: event.target.value } })} /></label>
      <label>{t('glossary.targetLocale')}<input className="tr-select" value={term.target_locale.tag} onChange={event => setTerm({ ...term, target_locale: { tag: event.target.value } })} /></label>
      <label>{t('glossary.sourceText')}<input className="tr-select" value={term.source_text} onChange={event => setTerm({ ...term, source_text: event.target.value })} /></label>
      <label>{t('glossary.targetText')}<input className="tr-select" value={term.target_text} onChange={event => setTerm({ ...term, target_text: event.target.value })} /></label>
      <label>{t('glossary.aliases')}<textarea className="tr-select" value={term.aliases.join('\n')} onChange={event => setTerm({ ...term, aliases: event.target.value.split('\n') })} /></label>
      <label>{t('glossary.context')}<input className="tr-select" value={term.context ?? ''} onChange={event => setTerm({ ...term, context: event.target.value || null })} /></label>
      <label>{t('glossary.resourceKey')}<input className="tr-select" value={term.resource_key ?? ''} onChange={event => setTerm({ ...term, resource_key: event.target.value || null })} /></label>
      <label>{t('glossary.disambiguation')}<input className="tr-select" value={term.disambiguation} onChange={event => setTerm({ ...term, disambiguation: event.target.value })} /></label>
      <div className="tr-actions"><button className="tr-button tr-primary" disabled={busy} onClick={() => void act(async () => {
        await api.saveTerm(term); await refreshResources(); setTerm(EMPTY_TERM); setDrawer(null)
        if (selectedWork !== null) setPreflight(await api.openWork(selectedWork))
      })}>{term.id === null ? t('glossary.create') : t('glossary.save')}</button><button className="tr-button" onClick={() => setDrawer(null)}>{t('common.cancel')}</button></div>
    </div></aside></div>}
  </div>
}
