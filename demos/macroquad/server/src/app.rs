use std::{collections::HashMap, time::Duration};

use naia_server::{
    shared::{Random, Timer},
    Event, RoomKey, Server as NaiaServer, ServerAddrs, ServerConfig, UserKey,
};

use naia_demo_world::{Entity, World as DemoWorld};

use naia_macroquad_demo_shared::{
    behavior as shared_behavior,
    protocol::{Color, EntityAssignment, Marker, Protocol, Square},
    shared_config, Channels,
};
use naia_macroquad_demo_shared::protocol::KeyCommand;

type World = DemoWorld<Protocol>;
type Server = NaiaServer<Protocol, Entity, Channels>;

struct LastCommand {
    pub w: bool,
    pub s: bool,
    pub a: bool,
    pub d: bool,
}

pub struct App {
    server: Server,
    world: World,
    main_room_key: RoomKey,
    user_squares: HashMap<UserKey, Entity>,
    square_last_command: HashMap<Entity, KeyCommand>,
    bandwidth_timer: Timer,
    other_main_entity: Entity,
}

impl App {
    pub fn new() -> Self {
        info!("Naia Macroquad Server Demo started");

        let server_addresses = ServerAddrs::new(
            "127.0.0.1:14191"
                .parse()
                .expect("could not parse Signaling address/port"),
            // IP Address to listen on for UDP WebRTC data channels
            "127.0.0.1:14192"
                .parse()
                .expect("could not parse WebRTC data address/port"),
            // The public WebRTC IP address to advertise
            "http://127.0.0.1:14192",
        );

        let mut server_config = ServerConfig::default();

        server_config.connection.bandwidth_measure_duration = Some(Duration::from_secs(4));

        let mut server = Server::new(&server_config, &shared_config());
        server.listen(&server_addresses);

        let mut world = World::new();

        // Create a new, singular room, which will contain Users and Entities that they
        // can receive updates from
        let main_room_key = server.make_room().key();

        // Create a new main entity that we can use for testing
        let other_main_entity = server
            .spawn_entity(world.proxy_mut())
            .enter_room(&main_room_key)
            .id();

        App {
            server,
            world,
            main_room_key,
            user_squares: HashMap::new(),
            square_last_command: HashMap::new(),
            bandwidth_timer: Timer::new(Duration::from_secs(4)),
            other_main_entity,
        }
    }

    pub fn update(&mut self) {
        if self.bandwidth_timer.ringing() {
            self.bandwidth_timer.reset();

            info!(
                "Bandwidth: {} kbps outgoing",
                self.server.outgoing_bandwidth_total()
            );
        }

        for event in self.server.receive() {
            match event {
                Ok(Event::Authorization(user_key, Protocol::Auth(auth))) => {
                    let ref username = *auth.username;
                    let ref password = *auth.password;
                    if username == "charlie" && password == "12345" {
                        // Accept incoming connection
                        self.server.accept_connection(&user_key);
                    } else {
                        // Reject incoming connection
                        self.server.reject_connection(&user_key);
                    }
                }
                Ok(Event::Connection(user_key)) => {
                    // New User has joined the Server
                    let user_address = self
                        .server
                        .user_mut(&user_key)
                        // User enters a Room to see the contained Entities
                        .enter_room(&self.main_room_key)
                        .address();

                    info!("Naia Server connected to: {}", user_address);

                    let total_user_count = self.server.users_count();

                    // Spawn new Entity
                    let mut entity = self.server.spawn_entity(self.world.proxy_mut());

                    // Create "Square" Component
                    let x = Random::gen_range_u32(0, 50) * 16;
                    let y = Random::gen_range_u32(0, 37) * 16;

                    let square_color = match total_user_count % 3 {
                        0 => Color::Yellow,
                        1 => Color::Red,
                        2 => Color::Blue,
                        _ => Color::Green,
                    };

                    // Get Entity ID
                    let entity_id = entity
                        // Entity enters Room
                        .enter_room(&self.main_room_key)
                        // Add Square component to Entity
                        .insert_component(Square::new(x as u16, y as u16, square_color))
                        // Add Marker component to Entity
                        .insert_component(Marker::new())
                        .id();

                    // Associate new Entity with User that spawned it
                    self.user_squares.insert(user_key, entity_id);
                    self.square_last_command.insert(entity_id, KeyCommand::new(false, false, false, false));

                    // Send an Entity Assignment message to the User that owns the Square
                    let mut assignment_message = EntityAssignment::new(true);
                    assignment_message.entity.set(&self.server, &entity_id);
                    //eventually would like to do this like:
                    //self.server.entity_property(assigment_message).set(&entity_id);
                    assignment_message
                        .other_entity
                        .set(&self.server, &self.other_main_entity);

                    self.server.send_message(
                        &user_key,
                        Channels::EntityAssignment,
                        &assignment_message,
                    );
                }
                Ok(Event::Disconnection(user_key, user)) => {
                    info!("Naia Server disconnected from: {}", user.address);
                    if let Some(entity) = self.user_squares.remove(&user_key) {
                        self.server
                            .entity_mut(self.world.proxy_mut(), &entity)
                            .leave_room(&self.main_room_key)
                            .despawn();
                        self.square_last_command.remove(&entity);
                    }
                }
                Ok(Event::Message(
                    _,
                    Channels::PlayerCommand,
                    Protocol::KeyCommand(key_command),
                )) => {
                    if let Some(entity) = key_command.entity.get(&self.server) {
                        self.square_last_command.insert(entity, key_command);
                    }
                }
                Ok(Event::Tick) => {
                    // All game logic should happen here, on a tick event

                    // Check whether Entities are in/out of all possible Scopes
                    for (_, user_key, entity) in self.server.scope_checks() {
                        // You'd normally do whatever checks you need to in here..
                        // to determine whether each Entity should be in scope or not.

                        // This indicates the Entity should be in this scope.
                        self.server.user_scope(&user_key).include(&entity);

                        // And call this if Entity should NOT be in this scope.
                        // self.server.user_scope(..).exclude(..);
                    }

                    for (entity, last_command) in self.square_last_command.drain() {
                        if let Some(mut square) = self
                            .server
                            .entity_mut(self.world.proxy_mut(), &entity)
                            .component::<Square>()
                        {
                            shared_behavior::process_command(&last_command, &mut square);
                        }
                    }

                    // VERY IMPORTANT! Calling this actually sends all update data
                    // packets to all Clients that require it. If you don't call this
                    // method, the Server will never communicate with it's connected Clients
                    self.server.send_all_updates(self.world.proxy());
                }
                Err(error) => {
                    info!("Naia Server error: {}", error);
                }
                _ => {}
            }
        }
    }
}
