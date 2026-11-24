import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import { useTranslation } from 'react-i18next';

/**
 * 목록에서 하나를 고르는 입력. 기본 select 대신 화면 디자인에 맞춘 목록을 그린다.
 * Select는 버튼(combobox)으로 열고, SearchSelect는 입력칸에 글자를 쳐서 목록을 좁힌다.
 * 고른 값은 숨은 입력(type=hidden)으로 폼 전송(FormData)에 그대로 들어간다.
 */
export type SelectOption = { value: string; label: string; disabled?: boolean };
export type SelectProps = {
  options: SelectOption[];
  id?: string;
  name?: string;
  /** 제어 모드. 주면 onChange로 바뀐 값을 받아 다시 넘겨야 한다. */
  value?: string;
  /** 비제어 모드의 처음 값. */
  defaultValue?: string;
  onChange?: (value: string) => void;
  disabled?: boolean;
  /** 빈 값이면 폼 제출을 막고 입력칸을 오류로 표시한다. */
  required?: boolean;
  /** 고른 값이 목록에 없을 때 보이는 안내 글. */
  placeholder?: string;
  /** 열린 목록의 최대 높이(px). 넘치면 목록 안에서 스크롤한다. */
  maxHeight?: number;
  'aria-label'?: string;
  'aria-describedby'?: string;
};

const ESTIMATED_OPTION_HEIGHT = 36;

function firstEnabled(options: SelectOption[], from: number, step: 1 | -1): number {
  for (let i = from; i >= 0 && i < options.length; i += step) if (!options[i].disabled) return i;
  return -1;
}

/** 제어·비제어 값, 폼 reset, 필수 입력 오류 표시를 함께 다룬다. */
function useSelectValue(props: SelectProps, hidden: RefObject<HTMLInputElement | null>) {
  const controlled = props.value !== undefined;
  const [inner, setInner] = useState(props.defaultValue ?? '');
  const [invalid, setInvalid] = useState(false);
  const value = controlled ? props.value! : inner;
  const initial = useRef(props.defaultValue ?? '');
  useEffect(() => {
    const form = hidden.current?.form;
    if (!form || controlled) return;
    const reset = () => { setInner(initial.current); setInvalid(false); };
    form.addEventListener('reset', reset);
    return () => form.removeEventListener('reset', reset);
  }, [controlled, hidden]);
  const commit = (next: string) => {
    if (!controlled) setInner(next);
    if (next) setInvalid(false);
    if (next !== value) props.onChange?.(next);
  };
  return { value, commit, invalid, setInvalid };
}

/** 열린 목록의 위치. 아래 공간이 모자라고 위가 더 넓으면 위로 연다. 모달 안에서도 잘리지 않게 화면 기준(fixed)으로 둔다. */
function usePopupPosition(open: boolean, anchor: RefObject<HTMLElement | null>, maxHeight: number, count: number) {
  const [style, setStyle] = useState<{ popup: CSSProperties; list: CSSProperties }>({ popup: {}, list: { maxHeight } });
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const node = anchor.current;
      if (!node) return;
      const rect = node.getBoundingClientRect(), viewport = window.innerHeight || document.documentElement.clientHeight;
      const below = viewport - rect.bottom - 8, above = rect.top - 8;
      const needed = Math.min(maxHeight, Math.max(1, count) * ESTIMATED_OPTION_HEIGHT + 10);
      const up = below < needed && above > below;
      const room = Math.max(96, (up ? above : below) - 10);
      const popup: CSSProperties = { position: 'fixed', left: rect.left, minWidth: rect.width, maxWidth: `calc(100vw - ${Math.max(0, rect.left) + 8}px)` };
      if (up) popup.bottom = viewport - rect.top + 4; else popup.top = rect.bottom + 4;
      setStyle({ popup, list: { maxHeight: Math.min(maxHeight, room) } });
    };
    place();
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => { window.removeEventListener('resize', place); window.removeEventListener('scroll', place, true); };
  }, [open, anchor, maxHeight, count]);
  return style;
}

/** 목록 밖을 누르면 닫는다. */
function useOutsideClose(open: boolean, root: RefObject<HTMLElement | null>, close: () => void) {
  const latest = useRef(close);
  latest.current = close;
  useEffect(() => {
    if (!open) return;
    const down = (event: MouseEvent) => { if (!root.current?.contains(event.target as Node)) latest.current(); };
    document.addEventListener('mousedown', down, true);
    return () => document.removeEventListener('mousedown', down, true);
  }, [open, root]);
}

function Check() {
  return <svg className="icon select-check" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M3.5 8.5l3 3 6-6.5" /></svg>;
}

type ListProps = { id: string; options: SelectOption[]; value: string; /** 여러 개를 고르는 목록이면 고른 값 전부. */ values?: readonly string[]; active: number; optionId: (index: number) => string; onActive: (index: number) => void; onChoose: (option: SelectOption) => void; style: { popup: CSSProperties; list: CSSProperties }; empty?: string };
function List({ id, options, value, values, active, optionId, onActive, onChoose, style, empty }: ListProps) {
  const list = useRef<HTMLUListElement>(null);
  useEffect(() => {
    if (active < 0) return;
    const node = list.current?.querySelector<HTMLElement>(`[data-index="${active}"]`);
    node?.scrollIntoView?.({ block: 'nearest' });
  }, [active]);
  // 목록을 눌러도 입력(버튼·입력칸)의 초점을 잃지 않게 한다.
  return <div className="select-popup" style={style.popup} onMouseDown={event => event.preventDefault()}>
    <ul ref={list} id={id} role="listbox" aria-multiselectable={values ? true : undefined} className="select-list" style={style.list}>
      {options.map((option, index) => {
        const selected = values ? values.includes(option.value) : option.value === value;
        return <li key={option.value} id={optionId(index)} data-index={index} data-value={option.value} role="option" aria-selected={selected} aria-disabled={option.disabled || undefined}
          className={index === active ? 'select-option active' : 'select-option'}
          onMouseMove={() => { if (!option.disabled && index !== active) onActive(index); }}
          onClick={() => { if (!option.disabled) onChoose(option); }}>
          <span className="select-option-label">{option.label}</span>{selected && <Check />}
        </li>;
      })}
    </ul>
    {empty && options.length === 0 && <p className="select-empty">{empty}</p>}
  </div>;
}

/** 필수 입력 검사용 보이지 않는 입력. 숨은 입력(type=hidden)은 브라우저 검사를 받지 않아 따로 둔다. 이름이 없어 전송되지 않는다. */
function Validity({ value, disabled, onInvalid, focusTarget }: { value: string; disabled?: boolean; onInvalid: () => void; focusTarget: RefObject<HTMLElement | null> }) {
  return <input className="select-validity" tabIndex={-1} aria-hidden="true" required disabled={disabled} value={value} onChange={() => {}} onInvalid={onInvalid} onFocus={() => focusTarget.current?.focus()} />;
}

/** 버튼으로 여는 목록 선택. 화살표·Home/End·Enter·Esc·Tab과 첫 글자 이동(type-ahead)을 지원한다. */
export function Select(props: SelectProps) {
  const { options, id, name, disabled, required, placeholder, maxHeight = 320 } = props;
  const auto = useId(), listId = `${auto}-list`, optionId = (index: number) => `${auto}-option-${index}`;
  const root = useRef<HTMLDivElement>(null), trigger = useRef<HTMLButtonElement>(null), hidden = useRef<HTMLInputElement>(null);
  const { value, commit, invalid, setInvalid } = useSelectValue(props, hidden);
  const [open, setOpen] = useState(false), [active, setActive] = useState(-1);
  const typed = useRef(''), typedAt = useRef(0);
  const selectedIndex = options.findIndex(option => option.value === value);
  const selected = selectedIndex >= 0 ? options[selectedIndex] : undefined;
  const style = usePopupPosition(open, trigger, maxHeight, options.length);
  const close = () => { setOpen(false); setActive(-1); };
  useOutsideClose(open, root, close);
  const show = (index = selectedIndex >= 0 && !options[selectedIndex].disabled ? selectedIndex : firstEnabled(options, 0, 1)) => { if (disabled) return; setOpen(true); setActive(index); };
  const choose = (option: SelectOption) => { commit(option.value); close(); };
  const typeAhead = (key: string) => {
    const now = Date.now();
    typed.current = now - typedAt.current > 700 ? key : typed.current + key;
    typedAt.current = now;
    const text = typed.current, repeated = text.length > 1 && [...text].every(c => c === text[0]);
    const prefix = repeated ? text[0] : text, from = open ? active : selectedIndex;
    const start = repeated || text.length === 1 ? from + 1 : Math.max(from, 0);
    for (let n = 0; n < options.length; n++) {
      const index = (start + n + options.length) % options.length, option = options[index];
      if (!option.disabled && option.label.toLowerCase().startsWith(prefix)) { show(index); return; }
    }
  };
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    const key = event.key;
    if (key === 'Tab') { if (open) close(); return; }
    if (key.length === 1 && key !== ' ' && !event.ctrlKey && !event.metaKey && !event.altKey) { event.preventDefault(); typeAhead(key.toLowerCase()); return; }
    if (key === ' ' && Date.now() - typedAt.current < 700 && typed.current) { event.preventDefault(); typeAhead(' '); return; }
    if (!open) {
      if (key === 'ArrowDown' || key === 'ArrowUp' || key === 'Enter' || key === ' ') { event.preventDefault(); show(); }
      else if (key === 'Home') { event.preventDefault(); show(firstEnabled(options, 0, 1)); }
      else if (key === 'End') { event.preventDefault(); show(firstEnabled(options, options.length - 1, -1)); }
      return;
    }
    if (key === 'ArrowDown') { event.preventDefault(); if (event.altKey) return; const next = firstEnabled(options, active + 1, 1); if (next >= 0) setActive(next); }
    else if (key === 'ArrowUp') { event.preventDefault(); if (event.altKey) { if (active >= 0) choose(options[active]); return; } const prev = firstEnabled(options, active - 1, -1); if (prev >= 0) setActive(prev); }
    else if (key === 'Home') { event.preventDefault(); setActive(firstEnabled(options, 0, 1)); }
    else if (key === 'End') { event.preventDefault(); setActive(firstEnabled(options, options.length - 1, -1)); }
    else if (key === 'Enter' || key === ' ') { event.preventDefault(); if (active >= 0) choose(options[active]); else close(); }
    else if (key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); }
  };
  return <div ref={root} className="select-box" onBlur={event => { if (open && !root.current?.contains(event.relatedTarget as Node | null)) close(); }}>
    <button ref={trigger} type="button" role="combobox" id={id} className="select-trigger" value={value} data-value={value}
      aria-haspopup="listbox" aria-expanded={open} aria-controls={open ? listId : undefined}
      aria-activedescendant={open && active >= 0 ? optionId(active) : undefined}
      aria-label={props['aria-label']} aria-describedby={props['aria-describedby']} aria-required={required || undefined} aria-invalid={invalid || undefined}
      disabled={disabled}
      onClick={() => (open ? close() : show())}
      onKeyDown={onKeyDown}
      onKeyUp={event => { if (event.key === ' ') event.preventDefault(); }}>
      <span className={selected ? 'select-value' : 'select-value select-placeholder'}>{selected ? selected.label : placeholder ?? ''}</span>
    </button>
    <input ref={hidden} type="hidden" name={name} value={value} disabled={disabled} />
    {required && <Validity value={value} disabled={disabled} onInvalid={() => setInvalid(true)} focusTarget={trigger} />}
    {open && <List id={listId} options={options} value={value} active={active} optionId={optionId} onActive={setActive} onChoose={choose} style={style} />}
  </div>;
}

/** 글자를 쳐서 목록을 좁히는 선택. 이름(또는 값)에 친 글자가 들어 있는 항목만 보이고, 초점을 잃으면 고른 항목 이름으로 되돌린다. */
export function SearchSelect(props: SelectProps) {
  const { t } = useTranslation();
  const { options, id, name, disabled, required, placeholder, maxHeight = 320 } = props;
  const auto = useId(), listId = `${auto}-list`, optionId = (index: number) => `${auto}-option-${index}`;
  const root = useRef<HTMLDivElement>(null), input = useRef<HTMLInputElement>(null), hidden = useRef<HTMLInputElement>(null);
  const { value, commit, invalid, setInvalid } = useSelectValue(props, hidden);
  const [open, setOpen] = useState(false), [active, setActive] = useState(-1), [query, setQuery] = useState<string | null>(null);
  const selected = options.find(option => option.value === value);
  const needle = (query ?? '').trim().toLowerCase();
  const shown = needle ? options.filter(option => option.label.toLowerCase().includes(needle) || option.value.toLowerCase().includes(needle)) : options;
  const style = usePopupPosition(open, input, maxHeight, shown.length);
  const close = () => { setOpen(false); setActive(-1); setQuery(null); };
  useOutsideClose(open, root, close);
  const show = (list = shown) => { if (disabled) return; const index = list.findIndex(option => option.value === value && !option.disabled); setOpen(true); setActive(index >= 0 ? index : firstEnabled(list, 0, 1)); };
  const choose = (option: SelectOption) => { commit(option.value); close(); };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    const key = event.key;
    if (key === 'Tab') { if (open) close(); return; }
    if (!open) {
      if (key === 'ArrowDown' || key === 'ArrowUp') { event.preventDefault(); show(); }
      return;
    }
    if (key === 'ArrowDown') { event.preventDefault(); const next = firstEnabled(shown, active + 1, 1); if (next >= 0) setActive(next); }
    else if (key === 'ArrowUp') { event.preventDefault(); const prev = firstEnabled(shown, active - 1, -1); if (prev >= 0) setActive(prev); }
    else if (key === 'Home') { event.preventDefault(); setActive(firstEnabled(shown, 0, 1)); }
    else if (key === 'End') { event.preventDefault(); setActive(firstEnabled(shown, shown.length - 1, -1)); }
    else if (key === 'Enter') { event.preventDefault(); if (active >= 0 && shown[active]) choose(shown[active]); }
    else if (key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); }
  };
  return <div ref={root} className="select-box" onBlur={event => { if (!root.current?.contains(event.relatedTarget as Node | null)) close(); }}>
    <input ref={input} id={id} type="text" role="combobox" className="select-search" autoComplete="off" spellCheck={false} data-value={value}
      aria-autocomplete="list" aria-expanded={open} aria-controls={open ? listId : undefined}
      aria-activedescendant={open && active >= 0 && shown[active] ? optionId(active) : undefined}
      aria-label={props['aria-label']} aria-describedby={props['aria-describedby']} aria-required={required || undefined} aria-invalid={invalid || undefined}
      disabled={disabled} placeholder={placeholder}
      value={query ?? selected?.label ?? ''}
      onChange={event => {
        const text = event.target.value, low = text.trim().toLowerCase();
        const next = low ? options.filter(option => option.label.toLowerCase().includes(low) || option.value.toLowerCase().includes(low)) : options;
        setQuery(text); setOpen(true); setActive(firstEnabled(next, 0, 1));
      }}
      onFocus={event => event.currentTarget.select()}
      onClick={() => { if (!open) show(); }}
      onKeyDown={onKeyDown} />
    <input ref={hidden} type="hidden" name={name} value={value} disabled={disabled} />
    {required && <Validity value={value} disabled={disabled} onInvalid={() => setInvalid(true)} focusTarget={input} />}
    {open && <List id={listId} options={shown} value={value} active={active} optionId={optionId} onActive={setActive} onChoose={choose} style={style} empty={t('결과 없음')} />}
  </div>;
}

/** 여러 개를 고르는 목록의 항목. group이 같은 항목은 하나만 고를 수 있어, 새로 고르면 같은 group의 이전 선택이 빠진다. */
export type MultiSelectOption = SelectOption & { group?: string; /** 고른 뒤 배지에 보일 내용. 없으면 label을 보인다. 배지의 제거 버튼 이름은 언제나 label을 쓴다. */ chip?: ReactNode };
export type MultiSelectProps = {
  options: MultiSelectOption[];
  /** 고른 값. 고른 순서대로 배지로 보인다. */
  value: readonly string[];
  onChange: (value: string[]) => void;
  id?: string;
  disabled?: boolean;
  placeholder?: string;
  maxHeight?: number;
  'aria-label'?: string;
  'aria-describedby'?: string;
};

/**
 * 여러 개를 고르는 선택. 고른 항목은 입력칸 안에 배지로 보이고, 배지의 × 버튼이나 목록에서 다시 눌러 뺀다.
 * 글자를 치면 목록이 좁혀지고, 빈 입력칸에서 Backspace를 누르면 마지막 배지가 빠진다. 고른 뒤에도 목록은 열려 있다.
 */
export function MultiSelect(props: MultiSelectProps) {
  const { t } = useTranslation();
  const { options, value, onChange, id, disabled, placeholder, maxHeight = 320 } = props;
  const auto = useId(), listId = `${auto}-list`, optionId = (index: number) => `${auto}-option-${index}`;
  const root = useRef<HTMLDivElement>(null), input = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false), [active, setActive] = useState(-1), [query, setQuery] = useState('');
  const needle = query.trim().toLowerCase();
  const shown = needle ? options.filter(option => option.label.toLowerCase().includes(needle) || option.value.toLowerCase().includes(needle)) : options;
  const style = usePopupPosition(open, root, maxHeight, shown.length);
  const close = () => { setOpen(false); setActive(-1); setQuery(''); };
  useOutsideClose(open, root, close);
  const show = () => { if (disabled) return; setOpen(true); setActive(firstEnabled(shown, 0, 1)); };
  const remove = (target: string) => onChange(value.filter(v => v !== target));
  const toggle = (option: MultiSelectOption) => {
    if (value.includes(option.value)) { remove(option.value); return; }
    // 같은 group의 이전 선택은 새 선택으로 바뀐다.
    const kept = option.group === undefined ? value : value.filter(v => options.find(o => o.value === v)?.group !== option.group);
    onChange([...kept, option.value]); setQuery('');
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    const key = event.key;
    if (key === 'Tab') { if (open) close(); return; }
    if (key === 'Backspace' && !query && value.length) { remove(value[value.length - 1]); return; }
    if (!open) {
      if (key === 'ArrowDown' || key === 'ArrowUp') { event.preventDefault(); show(); }
      return;
    }
    if (key === 'ArrowDown') { event.preventDefault(); const next = firstEnabled(shown, active + 1, 1); if (next >= 0) setActive(next); }
    else if (key === 'ArrowUp') { event.preventDefault(); const prev = firstEnabled(shown, active - 1, -1); if (prev >= 0) setActive(prev); }
    else if (key === 'Home') { event.preventDefault(); setActive(firstEnabled(shown, 0, 1)); }
    else if (key === 'End') { event.preventDefault(); setActive(firstEnabled(shown, shown.length - 1, -1)); }
    else if (key === 'Enter') { event.preventDefault(); if (active >= 0 && shown[active]) toggle(shown[active]); }
    else if (key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); }
  };
  return <div ref={root} className={disabled ? 'select-box multi-select disabled' : 'select-box multi-select'} data-value={value.join(',')}
    onBlur={event => { if (!root.current?.contains(event.relatedTarget as Node | null)) close(); }}
    onMouseDown={event => { if (event.target === event.currentTarget) { event.preventDefault(); input.current?.focus(); if (!open) show(); } }}>
    {value.map(v => { const option = options.find(o => o.value === v), label = option?.label ?? v; return <span key={v} className="chip">{option?.chip ?? label}
      <button type="button" className="chip-remove" disabled={disabled} aria-label={t('{{name}} 제거', { name: label })} onClick={() => { remove(v); input.current?.focus(); }}>
        <svg className="icon" width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" aria-hidden="true"><path d="M3 3l6 6M9 3l-6 6" /></svg>
      </button></span>; })}
    <input ref={input} id={id} type="text" role="combobox" className="multi-select-input" autoComplete="off" spellCheck={false} data-value={value.join(',')}
      aria-autocomplete="list" aria-expanded={open} aria-controls={open ? listId : undefined}
      aria-activedescendant={open && active >= 0 && shown[active] ? optionId(active) : undefined}
      aria-label={props['aria-label']} aria-describedby={props['aria-describedby']}
      disabled={disabled} placeholder={value.length ? undefined : placeholder} value={query}
      onChange={event => { const text = event.target.value, low = text.trim().toLowerCase(); const next = low ? options.filter(option => option.label.toLowerCase().includes(low) || option.value.toLowerCase().includes(low)) : options; setQuery(text); setOpen(true); setActive(firstEnabled(next, 0, 1)); }}
      onClick={() => { if (!open) show(); }}
      onKeyDown={onKeyDown} />
    {open && <List id={listId} options={shown} value="" values={value} active={active} optionId={optionId} onActive={setActive} onChoose={toggle} style={style} empty={t('결과 없음')} />}
  </div>;
}
