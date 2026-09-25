import type { ColumnMapping, CsvInspection, CsvPage, Preflight, TranslationExport, MemoryEntry, GlossaryTerm, TermInput, ProviderConfigInput, JobLimits, JobProgress, WorkTree, ProviderProfile } from './types.generated'
import './style.css'
export { TranslatorPage } from './Workspace'
export interface TranslatorApi {
  workTree(): Promise<WorkTree>
  renameWork(workId: number, name: string): Promise<void>
  openWork(workId: number): Promise<Preflight>
  csvPage(workId: number, jobId: number | null, offset: number, limit: number): Promise<CsvPage>
  workJobProgress(jobId: number): Promise<JobProgress>
  exportWorkDrafts(jobId: number): Promise<TranslationExport>
  providerProfiles(): Promise<ProviderProfile[]>
  saveProfile(id: number | null, name: string, input: ProviderConfigInput): Promise<ProviderProfile>
  activateProfile(id: number): Promise<ProviderProfile>
  deleteProfile(id: number): Promise<void>
  chooseFile(): Promise<CsvInspection | null>
  prepare(mapping: ColumnMapping): Promise<Preflight>
  export(): Promise<TranslationExport>
  memoryEntries(): Promise<MemoryEntry[]>
  disableMemory(id: number): Promise<void>
  glossaryTerms(): Promise<GlossaryTerm[]>
  saveTerm(input: TermInput): Promise<GlossaryTerm>
  disableTerm(id: number): Promise<void>
  jobStart(limits: JobLimits, onProgress: (progress: JobProgress) => void, onLog: (lines: string[]) => void): Promise<JobProgress>
  jobResumeSelected(jobId: number, limits: JobLimits, onProgress: (progress: JobProgress) => void, onLog: (lines: string[]) => void): Promise<JobProgress>
  jobPause(): Promise<void>
  jobCancel(): Promise<void>
}
