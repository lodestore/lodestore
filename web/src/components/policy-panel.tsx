import { CodeBlock } from "@/components/code-block";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";

function Snippet({
	text,
	object,
	loading,
}: {
	text?: string;
	object: string;
	loading: boolean;
}) {
	if (loading) return <Skeleton className="h-80 w-full" />;
	return (
		<ScrollArea className="h-80 rounded-md">
			<CodeBlock text={text ?? ""} object={object} disabled={!text} />
		</ScrollArea>
	);
}

export function PolicyPanel({
	iamJson,
	minioCommands,
	loading = false,
}: {
	iamJson?: string;
	minioCommands?: string;
	loading?: boolean;
}) {
	return (
		<section className="flex flex-col gap-2">
			<h3 className="text-lg font-semibold">{copy.policy.title}</h3>
			<p className="text-sm text-muted-foreground">{copy.policy.body}</p>
			<Tabs defaultValue="iam">
				<TabsList>
					<TabsTrigger value="iam">{copy.policy.tabIam}</TabsTrigger>
					<TabsTrigger value="minio">{copy.policy.tabMinio}</TabsTrigger>
				</TabsList>
				<TabsContent value="iam">
					<Snippet text={iamJson} object="IAM policy" loading={loading} />
				</TabsContent>
				<TabsContent value="minio">
					<Snippet
						text={minioCommands}
						object="MinIO commands"
						loading={loading}
					/>
				</TabsContent>
			</Tabs>
		</section>
	);
}
