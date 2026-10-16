import { Fragment } from "react";

export function PathText({ value }: { value: string }) {
	const units = value.match(/\/?[^/]*\/|[^/]+/g) ?? [];
	return (
		<span>
			{units.map((unit, i) => (
				<Fragment key={i}>
					<span className="inline-block max-w-full wrap-anywhere">{unit}</span>
					{unit.endsWith("/") && <wbr />}
				</Fragment>
			))}
		</span>
	);
}

export function TextWithPaths({ text }: { text: string }) {
	return (
		<span>
			{text
				.split(/(\S*\/\S*)/)
				.map((part, i) => (i % 2 ? <PathText key={i} value={part} /> : part))}
		</span>
	);
}
