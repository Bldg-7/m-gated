import { Fragment, useSyncExternalStore } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { ReadState, useResource } from '../admin/shared';
import { Badge } from '../../ui/ui';

/** OpenAPI 문서에서 이 화면이 읽는 부분(결정 0014 규칙 4). 문서는 서버가 `GET /api/openapi.json`으로 준다. */
export type Schema = {
  type?: string | string[]; properties?: Record<string, Schema>; required?: string[]; additionalProperties?: boolean | Schema;
  items?: Schema; enum?: unknown[]; const?: unknown; oneOf?: Schema[]; anyOf?: Schema[]; allOf?: Schema[]; $ref?: string;
  description?: string; format?: string; examples?: unknown[]; default?: unknown; minimum?: number;
};
type Parameter = { name: string; in: 'path' | 'query' | 'header' | 'cookie'; required?: boolean; description?: string; schema?: Schema };
type Response = { description?: string; 'x-error-codes'?: string[]; content?: Record<string, { schema?: Schema }> };
export type Operation = {
  operationId: string; tags?: string[]; summary?: string; description?: string; parameters?: Parameter[];
  requestBody?: { required?: boolean; content?: Record<string, { schema?: Schema }> };
  responses?: Record<string, Response>;
  'x-role'?: 'user' | 'admin'; 'x-token-scope'?: 'read' | 'write' | null; 'x-command-id'?: boolean; 'x-auth-mode'?: string;
};
export type ApiDocument = {
  info: { title: string; version: string; description?: string };
  tags?: { name: string; description?: string }[];
  paths: Record<string, Record<string, Operation | Parameter[]>>;
  components?: { schemas?: Record<string, Schema> };
  'x-common-errors'?: Record<string, string[]>;
};
const METHODS = ['get', 'post', 'put', 'patch', 'delete'] as const;
type Entry = { method: (typeof METHODS)[number]; path: string; operation: Operation; parameters: Parameter[] };

/** 문서의 모든 경로를 (메서드, 경로, 동작)으로 편다. 경로 단위 parameters는 동작의 것과 합친다. */
export function operations(doc: ApiDocument): Entry[] {
  const list: Entry[] = [];
  for (const [path, item] of Object.entries(doc.paths)) {
    const shared = Array.isArray(item.parameters) ? item.parameters : [];
    for (const method of METHODS) {
      const operation = item[method] as Operation | undefined;
      if (!operation) continue;
      const own = operation.parameters ?? [];
      list.push({ method, path, operation, parameters: [...shared.filter(p => !own.some(o => o.name === p.name && o.in === p.in)), ...own] });
    }
  }
  return list;
}
const refName = (ref: string) => ref.split('/').pop() ?? ref;
function resolve(doc: ApiDocument, schema: Schema): Schema {
  return schema.$ref ? doc.components?.schemas?.[refName(schema.$ref)] ?? {} : schema;
}
/** 스키마의 짧은 형 이름. 예: `string | null`, `Error`, `"read" | "write"`, `Token[]`. */
export function typeLabel(doc: ApiDocument, schema: Schema): string {
  if (schema.$ref) return refName(schema.$ref);
  if (schema.const !== undefined) return JSON.stringify(schema.const);
  if (schema.enum) return schema.enum.map(v => JSON.stringify(v)).join(' | ');
  const variants = schema.oneOf ?? schema.anyOf;
  if (variants) return variants.map(v => typeLabel(doc, v)).join(' | ');
  const types = Array.isArray(schema.type) ? schema.type : schema.type ? [schema.type] : [];
  return types.map(type => type === 'array' && schema.items ? `${typeLabel(doc, schema.items)}[]` : type).join(' | ') || (schema.allOf ? schema.allOf.map(v => typeLabel(doc, v)).join(' & ') : 'any');
}
/** curl 예시의 본문. 필수 속성만 넣고, 필수 속성이 없으면 모든 속성을 넣는다. null일 수 있는 값은 null로 둔다. */
export function example(doc: ApiDocument, input: Schema, depth = 0): unknown {
  const schema = resolve(doc, input);
  if (depth > 6) return null;
  if (schema.const !== undefined) return schema.const;
  if (schema.examples?.length) return schema.examples[0];
  if (schema.default !== undefined) return schema.default;
  if (schema.enum?.length) return schema.enum[0];
  const variants = schema.oneOf ?? schema.anyOf;
  if (variants?.length) return example(doc, variants.find(v => resolve(doc, v).type !== 'null') ?? variants[0], depth + 1);
  if (schema.allOf) return Object.assign({}, ...schema.allOf.map(v => example(doc, v, depth + 1)).filter(v => v && typeof v === 'object'));
  // 비워 둘 수 있는 값은 null로 둔다. 예시가 짧아지고, 그대로 보내도 맞는 본문이 된다.
  if (Array.isArray(schema.type) && schema.type.includes('null')) return null;
  const type = (Array.isArray(schema.type) ? schema.type[0] : schema.type) ?? (schema.properties ? 'object' : undefined);
  if (type === 'object') {
    const props = Object.entries(schema.properties ?? {});
    const required = schema.required ?? [];
    return Object.fromEntries(props.filter(([name]) => !required.length || required.includes(name)).map(([name, value]) => [name, example(doc, value, depth + 1)]));
  }
  if (type === 'array') return schema.items ? [example(doc, schema.items, depth + 1)] : [];
  if (type === 'integer' || type === 'number') return schema.minimum ?? 0;
  if (type === 'boolean') return false;
  if (type === 'string') return schema.format === 'uuid' ? '00000000-0000-4000-8000-000000000000' : schema.format === 'date-time' ? '2026-01-01T00:00:00Z' : 'string';
  return null;
}
const quote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
/** 관리 토큰으로 이 동작을 부르는 curl 명령. */
export function curl(doc: ApiDocument, entry: Entry, origin: string): string {
  const query = entry.parameters.filter(p => p.in === 'query' && p.required).map(p => `${p.name}=<${p.name}>`).join('&');
  const lines = [`curl -sS -X ${entry.method.toUpperCase()} ${quote(`${origin}${entry.path}${query ? `?${query}` : ''}`)}`, `-H "Authorization: Bearer $M_GATED_TOKEN"`];
  if (entry.operation['x-command-id']) lines.push(`-H "X-Gateway-Command-ID: $(uuidgen)"`);
  const schema = entry.operation.requestBody?.content?.['application/json']?.schema;
  if (schema) lines.push(`-H 'Content-Type: application/json'`, `-d ${quote(JSON.stringify(example(doc, schema)))}`);
  return lines.join(' \\\n  ');
}

const inline = (text: string) => text.split('`').map((part, i) => i % 2 ? <code key={i}>{part}</code> : <Fragment key={i}>{part}</Fragment>);
/**
 * 문서의 설명 글. 문서가 쓰는 Markdown의 일부만 그린다: `## 제목`, `- 목록`, 세 백틱으로 싼 코드 블록,
 * 빈 줄로 나눈 문단, 백틱 사이의 코드. 그 밖의 표기는 글자 그대로 보인다.
 */
export function Prose({ text }: { text?: string }) {
  if (!text) return null;
  const blocks: { kind: 'heading' | 'code' | 'list' | 'paragraph'; lines: string[] }[] = [];
  let fence = false;
  for (const line of text.split('\n')) {
    const last = blocks[blocks.length - 1];
    if (line.startsWith('```')) { fence = !fence; if (fence) blocks.push({ kind: 'code', lines: [] }); continue; }
    if (fence) { last.lines.push(line); continue; }
    if (!line.trim()) { blocks.push({ kind: 'paragraph', lines: [] }); continue; }
    if (line.startsWith('## ')) { blocks.push({ kind: 'heading', lines: [line.slice(3)] }); blocks.push({ kind: 'paragraph', lines: [] }); continue; }
    if (line.startsWith('- ')) { if (last?.kind === 'list') last.lines.push(line.slice(2)); else blocks.push({ kind: 'list', lines: [line.slice(2)] }); continue; }
    if (last?.kind === 'paragraph') last.lines.push(line); else blocks.push({ kind: 'paragraph', lines: [line] });
  }
  return <>{blocks.filter(b => b.lines.length > 0).map((b, i) =>
    b.kind === 'heading' ? <h3 key={i}>{inline(b.lines[0])}</h3>
      : b.kind === 'code' ? <pre key={i}>{b.lines.join('\n')}</pre>
        : b.kind === 'list' ? <ul key={i} className="api-prose api-list">{b.lines.map((item, j) => <li key={j}>{inline(item)}</li>)}</ul>
          : <p key={i} className="api-prose">{inline(b.lines.join(' '))}</p>)}</>;
}
/** 스키마를 속성 목록으로 그린다. 같은 이름의 스키마가 안에서 다시 나오면 이름만 보인다. */
function SchemaView({ doc, schema, seen = [] }: { doc: ApiDocument; schema: Schema; seen?: string[] }) {
  const { t } = useTranslation();
  const name = schema.$ref ? refName(schema.$ref) : null;
  if (name && seen.includes(name)) return null;
  const target = resolve(doc, schema), inside = name ? [...seen, name] : seen;
  const variants = target.oneOf ?? target.anyOf;
  if (variants) return <ul className="api-schema">{variants.map((v, i) => <li key={i}><span className="mono">{typeLabel(doc, v)}</span>{resolve(doc, v).description && <span className="meta"> — {inline(resolve(doc, v).description ?? '')}</span>}<SchemaView doc={doc} schema={v} seen={inside} /></li>)}</ul>;
  if (target.allOf) return <>{target.allOf.map((v, i) => <SchemaView key={i} doc={doc} schema={v} seen={inside} />)}</>;
  const items = target.items && (Array.isArray(target.type) ? target.type.includes('array') : target.type === 'array') ? target.items : null;
  if (items) return <SchemaView doc={doc} schema={items} seen={inside} />;
  const props = Object.entries(target.properties ?? {});
  if (!props.length) return null;
  return <ul className="api-schema">{props.map(([prop, value]) => {
    const inner = resolve(doc, value);
    return <li key={prop}>
      <code>{prop}</code> <span className="mono meta">{typeLabel(doc, value)}</span>{target.required?.includes(prop) && <> <span className="badge api-required">{t('필수')}</span></>}
      {(inner.description ?? value.description) && <div className="meta">{inline(value.description ?? inner.description ?? '')}</div>}
      <SchemaView doc={doc} schema={value} seen={inside} />
    </li>;
  })}</ul>;
}
const roleName = { admin: msgid('관리자만'), user: msgid('로그인한 사람') } as const;
/** 동작 하나의 설명: 권한, 매개변수, 본문, 응답, 오류 코드, curl 예시. */
function OperationView({ doc, entry }: { doc: ApiDocument; entry: Entry }) {
  const { t } = useTranslation();
  const { operation } = entry, scope = operation['x-token-scope'];
  const body = operation.requestBody?.content?.['application/json']?.schema;
  return <article className="card stack" aria-labelledby="api-operation-title">
    <h2 id="api-operation-title" className="api-operation-title"><span className={`api-method api-method-${entry.method}`}>{entry.method.toUpperCase()}</span>{' '}<code>{entry.path}</code></h2>
    {operation.summary && <p><strong>{inline(operation.summary)}</strong></p>}
    <div className="row">
      <Badge tone="neutral">{t(roleName[operation['x-role'] ?? 'user'])}</Badge>
      <Badge tone={scope ? 'info' : 'warning'}>{scope === 'read' ? t('읽기 토큰으로 부를 수 있음') : scope === 'write' ? t('쓰기 토큰이 필요함') : t('관리 토큰으로 부를 수 없음')}</Badge>
      {operation['x-command-id'] && <Badge tone="neutral">{t('명령 ID 필요')}</Badge>}
      {operation['x-auth-mode'] === 'password' && <Badge tone="neutral">{t('비밀번호 방식에만 있음')}</Badge>}
    </div>
    <Prose text={operation.description} />
    {entry.parameters.length > 0 && <section className="stack-sm"><h3>{t('매개변수')}</h3><div className="table-wrap"><table>
      <thead><tr><th scope="col">{t('이름')}</th><th scope="col">{t('위치')}</th><th scope="col">{t('형')}</th><th scope="col">{t('설명')}</th></tr></thead>
      <tbody>{entry.parameters.map(p => <tr key={`${p.in}:${p.name}`}><td><code>{p.name}</code>{p.required && <> <span className="badge api-required">{t('필수')}</span></>}</td><td>{p.in}</td><td className="mono">{p.schema ? typeLabel(doc, p.schema) : '-'}</td><td>{inline(p.description ?? '')}</td></tr>)}</tbody>
    </table></div></section>}
    {body && <section className="stack-sm"><h3>{t('요청 본문')}</h3><p className="mono meta">{typeLabel(doc, body)}</p><SchemaView doc={doc} schema={body} /></section>}
    <section className="stack-sm"><h3>{t('응답')}</h3>
      {Object.entries(operation.responses ?? {}).map(([status, response]) => {
        const schema = response.content?.['application/json']?.schema, codes = response['x-error-codes'];
        return <div key={status} className="api-response">
          <div className="row"><span className={`api-status api-status-${status[0]}`}>{status}</span><span>{inline(response.description ?? '')}</span></div>
          {codes?.length ? <p className="meta">{t('오류 코드')}: {codes.map((code, i) => <Fragment key={code}>{i > 0 && ', '}<code>{code}</code></Fragment>)}</p> : schema && <SchemaView doc={doc} schema={schema} />}
        </div>;
      })}
    </section>
    <section className="stack-sm"><h3>{t('curl 예시')}</h3>
      {scope ? <pre aria-label={t('curl 예시')}>{curl(doc, entry, window.location.origin)}</pre> : <p className="meta">{t('이 경로는 관리 토큰으로 부를 수 없습니다. 대시보드에서 쓰세요.')}</p>}
    </section>
  </article>;
}
function Overview({ doc }: { doc: ApiDocument }) {
  const { t } = useTranslation();
  const common = Object.entries(doc['x-common-errors'] ?? {});
  return <article className="card stack" aria-label={t('API 개요')}>
    <h2>{doc.info.title} <span className="meta">{t('버전 {{version}}', { version: doc.info.version })}</span></h2>
    <Prose text={doc.info.description} />
    {common.length > 0 && <section className="stack-sm"><h3>{t('모든 경로에 공통인 오류')}</h3>
      {common.map(([status, codes]) => <p key={status}><span className={`api-status api-status-${status[0]}`}>{status}</span> {codes.map((code, i) => <Fragment key={code}>{i > 0 && ', '}<code>{code}</code></Fragment>)}</p>)}
    </section>}
    <p><a href="/api/openapi.json" download="m-gated-management.openapi.json">{t('OpenAPI 문서(JSON) 내려받기')}</a></p>
  </article>;
}

/** 주소의 `#` 뒤. 화면 이동(경로·조회 문자열)과 달리 바뀌어도 화면을 다시 만들지 않으므로 문서를 다시 받지 않는다. */
function useHash() {
  return useSyncExternalStore(listener => { window.addEventListener('hashchange', listener); return () => window.removeEventListener('hashchange', listener); }, () => window.location.hash);
}
/** API 문서 화면. 왼쪽에 묶음별 경로 목록, 오른쪽에 고른 경로의 설명을 보인다. 고른 경로는 주소의 `#` 뒤에 operationId로 둔다. */
export function ApiReferenceScreen() {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const view = useResource<ApiDocument>('/api/openapi.json');
  const hash = useHash(), selected = hash.length > 1 ? decodeURIComponent(hash.slice(1)) : null;
  const doc = view.data, all = doc ? operations(doc) : [];
  const groups: [string, Entry[]][] = doc ? [...(doc.tags ?? []).map(tag => tag.name), ...all.flatMap(e => e.operation.tags ?? []).filter((name, i, list) => list.indexOf(name) === i)]
    .filter((name, i, list) => list.indexOf(name) === i)
    .map(name => [name, all.filter(e => (e.operation.tags?.[0] ?? '') === name && (auth.me?.role === 'admin' || e.operation['x-role'] !== 'admin'))] as [string, Entry[]])
    .filter(([, list]) => list.length > 0) : [];
  const current = all.find(e => e.operation.operationId === selected) ?? null;
  return <div className="stack-lg">
    <ReadState {...view} />
    {doc && <div className="api-ref">
      <nav aria-label={t('API 경로 목록')} className="api-nav">
        <a href="#" aria-current={current ? undefined : 'page'}>{t('개요')}</a>
        {groups.map(([name, list]) => <section key={name} className="stack-sm"><h3 className="nav-label">{name}</h3>
          <ul>{list.map(e => <li key={e.operation.operationId}><a href={`#${encodeURIComponent(e.operation.operationId)}`} aria-current={current === e ? 'page' : undefined}><span className={`api-method api-method-${e.method}`}>{e.method.toUpperCase()}</span>{' '}<span className="mono">{e.path}</span></a></li>)}</ul>
        </section>)}
      </nav>
      <div className="api-detail">{current ? <OperationView doc={doc} entry={current} /> : selected ? <p role="alert">{t('문서에 없는 경로입니다.')}</p> : <Overview doc={doc} />}</div>
    </div>}
  </div>;
}
