<script lang="ts">
	import CommitMessageEditor from '$components/v3/CommitMessageEditor.svelte';
	import Drawer from '$components/v3/Drawer.svelte';
	import { DiffService } from '$lib/hunks/diffService.svelte';
	import { lineIdsToHunkHeaders, type DiffHunk, type HunkHeader } from '$lib/hunks/hunk';
	import { showError, showToast } from '$lib/notifications/toasts';
	import { ChangeSelectionService, type SelectedHunk } from '$lib/selection/changeSelection.svelte';
	import {
		StackService,
		type CreateCommitRequestWorktreeChanges
	} from '$lib/stacks/stackService.svelte';
	import { UiState } from '$lib/state/uiState.svelte';
	import { TestId } from '$lib/testing/testIds';
	import { WorktreeService } from '$lib/worktree/worktreeService.svelte';
	import { getContext, inject } from '@gitbutler/shared/context';

	type Props = {
		projectId: string;
		stackId?: string;
	};
	const { projectId, stackId }: Props = $props();

	const stackService = getContext(StackService);
	const [uiState, worktreeService, diffService] = inject(UiState, WorktreeService, DiffService);
	const changeSelection = getContext(ChangeSelectionService);

	const [createCommitInStack, commitCreation] = stackService.createCommit;

	const stackState = $derived(stackId ? uiState.stack(stackId) : undefined);
	const selection = $derived(stackState?.selection.current);
	const selectedCommitId = $derived(selection?.commitId);

	const selectedChanges = $derived(changeSelection.list());
	const topBranchResult = $derived(stackId ? stackService.branches(projectId, stackId) : undefined);
	const topBranchName = $derived(topBranchResult?.current.data?.at(0)?.name);

	const draftBranchName = $derived(uiState.global.draftBranchName.current);

	const selectedBranchName = $derived(selection?.branchName || topBranchName);
	const canCommit = $derived(
		(selectedBranchName || draftBranchName || topBranchName) && selectedChanges.current.length > 0
	);
	const projectState = $derived(uiState.project(projectId));

	let input = $state<ReturnType<typeof CommitMessageEditor>>();
	let drawer = $state<ReturnType<typeof Drawer>>();

	async function findHunkDiff(filePath: string, hunk: SelectedHunk): Promise<DiffHunk | undefined> {
		const treeChange = await worktreeService.fetchChange(projectId, filePath);
		if (treeChange.data === undefined) {
			throw new Error('Failed to fetch change');
		}
		const changeDiff = await diffService.fetchDiff(projectId, treeChange.data);
		if (changeDiff.data === undefined) {
			throw new Error('Failed to fetch diff');
		}
		const file = changeDiff.data;

		if (file.type !== 'Patch') return undefined;

		const hunkDiff = file.subject.hunks.find(
			(hunkDiff) =>
				hunkDiff.oldStart === hunk.oldStart &&
				hunkDiff.oldLines === hunk.oldLines &&
				hunkDiff.newStart === hunk.newStart &&
				hunkDiff.newLines === hunk.newLines
		);
		return hunkDiff;
	}

	async function createCommit(message: string) {
		let finalStackId = stackId;
		let finalBranchName = selectedBranchName || topBranchName;

		if (!stackId) {
			const stack = await createNewStack({
				projectId,
				branch: { name: draftBranchName }
			});
			finalStackId = stack.id;
			projectState.stackId.set(finalStackId);
			finalBranchName = stack.heads[0]?.name; // Updated to access the name property
			uiState.global.draftBranchName.set(undefined);
		}

		if (!finalStackId) {
			throw new Error('No stack selected!');
		}

		if (!finalBranchName) {
			throw new Error('No branch selected!');
		}

		const worktreeChanges: CreateCommitRequestWorktreeChanges[] = [];

		for (const item of selectedChanges.current) {
			if (item.type === 'full') {
				worktreeChanges.push({
					pathBytes: item.pathBytes,
					previousPathBytes: item.previousPathBytes,
					hunkHeaders: []
				});
				continue;
			}

			if (item.type === 'partial') {
				const hunkHeaders: HunkHeader[] = [];
				for (const hunk of item.hunks) {
					if (hunk.type === 'full') {
						hunkHeaders.push(hunk);
						continue;
					}

					if (hunk.type === 'partial') {
						const hunkDiff = await findHunkDiff(item.path, hunk);
						if (!hunkDiff) {
							throw new Error('Hunk not found while commiting');
						}
						const selectedLines = hunk.lines;
						hunkHeaders.push(...lineIdsToHunkHeaders(selectedLines, hunkDiff.diff, 'commit'));
						continue;
					}
				}
				worktreeChanges.push({
					pathBytes: item.pathBytes,
					previousPathBytes: item.previousPathBytes,
					hunkHeaders
				});
				continue;
			}
		}

		const response = await createCommitInStack({
			projectId,
			stackId: finalStackId,
			parentId: selectedCommitId,
			message: message,
			stackBranchName: finalBranchName,
			worktreeChanges
		});

		const newId = response.newCommit;

		// Clear saved state for commit message editor.
		projectState.commitTitle.set('');
		projectState.commitDescription.set('');

		// Close the drawer.
		projectState.drawerPage.set(undefined);

		// Select the newly created commit.
		// Using `finalStackId` here because `stackState` might not have updated yet.
		uiState.stack(finalStackId).selection.set({ branchName: finalBranchName, commitId: newId });

		// Clear change/hunk selection used for creating the commit.
		changeSelection.clear();

		if (response.pathsToRejectedChanges.length > 0) {
			showError(
				'Some changes were not committed',
				'The following files were not committed becuase they are locked to another branch:\n' +
					response.pathsToRejectedChanges.map(([_reason, path]) => path).join('\n')
			);
		}
	}

	const [createNewStack, newStackResult] = stackService.newStack;

	async function handleCommitCreation() {
		const message = input?.getMessage();
		if (!message) {
			showToast({ message: 'Commit message is required', style: 'error' });
			return;
		}

		try {
			await createCommit(message);
		} catch (err: unknown) {
			showError('Failed to commit', err);
		}
	}

	function cancel() {
		drawer?.onClose();
	}
</script>

<Drawer
	testId={TestId.NewCommitDrawer}
	bind:this={drawer}
	{projectId}
	{stackId}
	title="Create commit"
	disableScroll
	minHeight={20}
>
	<CommitMessageEditor
		bind:this={input}
		{projectId}
		{stackId}
		actionLabel="Create commit"
		action={handleCommitCreation}
		onCancel={cancel}
		disabledAction={!canCommit}
		loading={commitCreation.current.isLoading || newStackResult.current.isLoading}
	/>
</Drawer>
