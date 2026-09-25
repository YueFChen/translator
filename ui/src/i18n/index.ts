import { createT } from './translate'
import { zh } from './zh-CN.ts'

/** 本插件资源表的 key 联合；写错 key 即编译错误。 */
export type MessageKey = keyof typeof zh & string

/** 本插件界面的解析函数。 */
export const t = createT(zh)
