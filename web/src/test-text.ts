/**
 * 화면에서 여러 요소로 나뉘어 그려진 글자(예: 금액의 흐린 0과 USD 단위)를 사용자가 읽는 그대로 찾는다.
 * 같은 글자를 가진 요소 중 가장 안쪽 하나만 일치시켜 부모·자식이 중복으로 잡히지 않게 한다.
 */
export function shownText(expected: string) {
  const normalize = (value: string | null | undefined) => (value ?? '').replace(/\s+/g, ' ').trim();
  return (_: string, element: Element | null) => !!element
    && normalize(element.textContent) === expected
    && !Array.from(element.children).some(child => normalize(child.textContent) === expected);
}
