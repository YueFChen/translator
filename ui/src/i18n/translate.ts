/**
 * 文案解析函数（B7）。
 *
 * 单语言期需要的能力就是「查表 + 插值」这一次函数调用；多语言、语言切换与回退链属内核规划
 * §3.2 的「本地化运行时」，等出现第二个语言再引入。所以这里不引第三方 i18n 依赖，也不经
 * `AppContext` 注入——模块导入即可。
 *
 * 约定见 docs/知识库早期调研与评测.md §8.1。
 */

/** 资源表：扁平 key → 文案。键形如 `settings.title`。 */
export type Messages = Record<string, string>

/** 宽泛的插值参数；只有表不是 `as const` 时才会用到。 */
export type MessageParams = Record<string, string | number>

/** 从模板里取出 `{name}` 的占位符名。 */
type Placeholder<T extends string> = T extends `${string}{${infer Name}}${infer Rest}`
  ? Name | Placeholder<Rest>
  : never

/**
 * 模板对应的参数类型。
 *
 * 占位符名写错即编译错误（`t('配置', { titel })` 不通过），未声明占位符的表不接受任何参数。
 * 表若没写 `as const`（模板退化成 `string`），就退回宽泛的 `MessageParams`——没有字面量信息
 * 可推导，只能放行。
 */
type ParamsFor<T extends string> = string extends T
  ? MessageParams
  : Record<Placeholder<T>, string | number>

/**
 * 绑定到某张资源表的解析函数。
 *
 * key 与 params 名都受表约束（`keyof M` / 模板占位符），写错即编译错误——这是「禁止字面量」
 * 之外另一半保证。
 */
export type Translator<M extends Messages> = <K extends keyof M & string>(
  key: K,
  params?: ParamsFor<M[K]>,
) => string

/** 告警去重：同一处（含同一 key 的同一参数集）只报一次，避免每次渲染刷屏。 */
const warned = new Set<string>()

const warnOnce = (reason: string, detail: string) => {
  const signature = `${reason}:${detail}`
  if (warned.has(signature)) return
  warned.add(signature)
  console.warn(`[i18n] ${reason}：${detail}`)
}

/**
 * 用资源表造一个解析函数。
 *
 * 缺 key 时返回 key 本身：界面不会出现空白，缺的是哪一条也一眼可见。
 * 占位符没被替换时保留原样并告警：漏掉的是哪个参数直接写在告警里。
 */
export function createT<M extends Messages>(messages: M): Translator<M> {
  return <K extends keyof M & string>(key: K, params?: ParamsFor<M[K]>): string => {
    const template: string | undefined = messages[key]
    if (template === undefined) {
      warnOnce('资源表里没有 key', key)
      return key
    }
    if (!params) return template
    const values = params as MessageParams
    const rendered = template.replace(/\{(\w+)\}/g, (placeholder, name: string) =>
      name in values ? String(values[name]) : placeholder,
    )
    // 未替换的占位符会把 `{name}` 直接渲染到界面上，这里兜一道：类型只管得住字面量调用。
    if (/\{\w+\}/.test(rendered)) warnOnce(`key ${key} 有占位符没被替换`, rendered)
    return rendered
  }
}
