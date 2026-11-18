/**
 * The screens: first-run setup, sign-in, the project list and the
 * workspace. The route is the `p` query parameter.
 */
import { For, Match, Show, Switch, createResource, createSignal, onCleanup, type JSX } from "solid-js"
import { api } from "./api"
import { Workspace } from "./Workspace"

const projectParam = (): string | null => new URLSearchParams(location.search).get("p")

export function App(): JSX.Element {
  const [project, setProject] = createSignal(projectParam())
  const [session, { refetch }] = createResource(api.session)
  const onPop = (): void => void setProject(projectParam())
  window.addEventListener("popstate", onPop)
  onCleanup(() => window.removeEventListener("popstate", onPop))
  const open = (id: string): void => {
    history.pushState(null, "", `?p=${id}`)
    setProject(id)
  }
  return (
    <Show when={session()} fallback={<p class="empty">Loading…</p>}>
      {(s) => (
        <Switch>
          <Match when={s().setup_needed}>
            <Setup done={() => void refetch()} />
          </Match>
          <Match when={!s().user}>
            <Login done={() => void refetch()} />
          </Match>
          <Match when={project()}>{(id) => <Workspace project={id()} />}</Match>
          <Match when={true}>
            <Projects open={open} />
          </Match>
        </Switch>
      )}
    </Show>
  )
}

/** A form that shows the error of its submit handler. */
function Form(props: { submit: (data: FormData) => Promise<void>; children: JSX.Element }): JSX.Element {
  const [error, setError] = createSignal("")
  const onSubmit = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault()
    setError("")
    try {
      await props.submit(new FormData(event.currentTarget as HTMLFormElement))
    } catch (e) {
      setError((e as Error).message)
    }
  }
  return (
    <form onSubmit={(e) => void onSubmit(e)}>
      {props.children}
      <p class="error" role="alert">
        {error()}
      </p>
    </form>
  )
}

const field = (data: FormData, name: string): string => String(data.get(name) ?? "")

function Setup(props: { done: () => void }): JSX.Element {
  return (
    <main class="narrow">
      <h1>Set up Nisaba</h1>
      <p>This server has no accounts yet. The account you create here owns the server.</p>
      <Form
        submit={async (d) => {
          await api.setup({ name: field(d, "name"), email: field(d, "email"), password: field(d, "password") })
          props.done()
        }}
      >
        <label>
          Name <input name="name" autocomplete="name" />
        </label>
        <label>
          Email <input name="email" type="email" required autocomplete="username" />
        </label>
        <label>
          Password <input name="password" type="password" minLength={8} required autocomplete="new-password" />
        </label>
        <button>Create account</button>
      </Form>
    </main>
  )
}

function Login(props: { done: () => void }): JSX.Element {
  return (
    <main class="narrow">
      <h1>Sign in</h1>
      <Form
        submit={async (d) => {
          await api.login({ name: "", email: field(d, "email"), password: field(d, "password") })
          props.done()
        }}
      >
        <label>
          Email <input name="email" type="email" required autocomplete="username" />
        </label>
        <label>
          Password <input name="password" type="password" required autocomplete="current-password" />
        </label>
        <button>Sign in</button>
      </Form>
    </main>
  )
}

function Projects(props: { open: (id: string) => void }): JSX.Element {
  const [list] = createResource(api.projects)
  return (
    <main class="narrow">
      <h1>Projects</h1>
      <Show when={list()?.length === 0}>
        <p class="empty">No projects yet. Start one below.</p>
      </Show>
      <ul class="projects">
        <For each={list()}>
          {(p) => (
            <li>
              <a href={`?p=${p.id}`} onClick={(e) => (e.preventDefault(), props.open(p.id))}>
                {p.name}
              </a>
            </li>
          )}
        </For>
      </ul>
      <Form
        submit={async (d) => {
          const { id } = await api.createProject({ name: field(d, "name"), files: [] })
          props.open(id)
        }}
      >
        <label>
          New project <input name="name" required placeholder="Name" />
        </label>
        <button>Create</button>
      </Form>
    </main>
  )
}
