import { createRoot } from 'react-dom/client'
import { useEffect } from 'react'
import { createPluginHostClient } from '@wonderland/plugin-ui-sdk'
import type { TranslatorApi } from './index'
import type { JobProgress } from './types.generated'
import { TranslatorPage } from './index'
import './host.css'

const host = createPluginHostClient('translator')
let activeJobRequest: string | null = null

async function jobCall(method: 'job_start' | 'job_resume_selected', params: Record<string, unknown>, onProgress: (progress: JobProgress) => void, onLog: (lines: string[]) => void) {
  await host.ready
  let requestId = ''
  const stopProgress = await host.subscribe('job.progress', event => { if (event.requestId === requestId) onProgress(event.payload as JobProgress) })
  const stopLog = await host.subscribe('job.log', event => {
    if (event.requestId === requestId && typeof event.payload === 'object' && event.payload !== null && Array.isArray((event.payload as { lines?: unknown }).lines)) onLog((event.payload as { lines: string[] }).lines)
  })
  const call = host.callWithId<JobProgress>(method, params)
  requestId = call.requestId
  activeJobRequest = requestId
  try { return await call.promise }
  finally {
    stopProgress(); stopLog()
    if (activeJobRequest === requestId) activeJobRequest = null
  }
}

const api: TranslatorApi = {
  workTree: () => host.call('work_tree'),
  renameWork: async (workId, name) => { await host.call('rename_work', { work_id: workId, name }) },
  openWork: (workId) => host.call('open_work', { work_id: workId }),
  csvPage: (workId, jobId, offset, limit) => host.call('csv_page', { work_id: workId, job_id: jobId, offset, limit }),
  workJobProgress: (jobId) => host.call('work_job_progress', { job_id: jobId }),
  exportWorkDrafts: (jobId) => host.call('export_work_drafts', { job_id: jobId }),
  providerProfiles: () => host.call('provider_profiles'),
  saveProfile: (id, name, input) => host.call('save_profile', { id, name, input }),
  activateProfile: (id) => host.call('activate_profile', { id }),
  deleteProfile: async (id) => { await host.call('delete_profile', { id }) },
  chooseFile: () => host.call('choose_file'),
  prepare: (mapping) => host.call('prepare', { mapping }),
  export: () => host.call('export'),
  memoryEntries: () => host.call('memory_entries'),
  disableMemory: async (id) => { await host.call('disable_memory', { id }) },
  glossaryTerms: () => host.call('glossary_terms'),
  saveTerm: (input) => host.call('save_term', { input }),
  disableTerm: async (id) => { await host.call('disable_term', { id }) },
  jobStart: (limits, onProgress, onLog) => jobCall('job_start', { limits }, onProgress, onLog),
  jobResumeSelected: (jobId, limits, onProgress, onLog) => jobCall('job_resume_selected', { job_id: jobId, limits }, onProgress, onLog),
  jobPause: async () => { await host.call('job_pause') },
  jobCancel: async () => { if (activeJobRequest) host.cancel(activeJobRequest) },
}

function App() {
  useEffect(() => {
    let disposed = false
    let stopTheme = () => {}
    let stopLifecycle = () => {}
    void host.followHostTheme(({ resolved }) => { document.documentElement.dataset.theme = resolved })
      .then(stop => { if (disposed) stop(); else stopTheme = stop })
    void host.onSurfaceLifecycle(state => { document.documentElement.dataset.surfaceState = state })
      .then(stop => { if (disposed) stop(); else stopLifecycle = stop })
    return () => { disposed = true; stopTheme(); stopLifecycle() }
  }, [])
  return <TranslatorPage api={api} />
}

createRoot(document.getElementById('root')!).render(<App />)
