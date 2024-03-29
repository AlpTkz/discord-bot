use lazy_static::lazy_static;
use redis;
use redis::{Commands, PipelineCommands};
use serenity::http::CacheHttp;
use serenity::model::{
    channel::PermissionOverwrite, channel::PermissionOverwriteType, id::ChannelId, id::GuildId,
    id::RoleId, id::UserId, permissions::Permissions,
};
use simple_error::SimpleError;
use white_rabbit;

// Test server:
pub const GUILD_ID: GuildId = GuildId(601070848446824509);
pub const ORGANIZER_ID: RoleId = RoleId(606829075226689536);
pub const GAME_MASTER_ID: Option<RoleId> = Some(RoleId(606913167439822987));
pub const ONE_SHOT_CATEGORY_ID: Option<ChannelId> = Some(ChannelId(607561808429056042));
pub const CAMPAIGN_CATEGORY_ID: Option<ChannelId> = Some(ChannelId(607561949651402772));
// SwissRPG:
// pub const GUILD_ID: GuildId = GuildId(401856510709202945);
// pub const ORGANIZER_ID: RoleId = RoleId(539447673988841492);
// pub const GAME_MASTER_ID: Option<RoleId> = Some(RoleId(412946716892069888));
// pub const ONE_SHOT_CATEGORY_ID: Option<ChannelId> = Some(ChannelId(562607292176924694));
// pub const CAMPAIGN_CATEGORY_ID: Option<ChannelId> = Some(ChannelId(414074722259828736));

lazy_static! {
    static ref EVENT_NAME_REGEX: regex::Regex =
        regex::Regex::new(r"^\s*(?P<name>[^\[\(]+[^\s\[\(])").unwrap();
}

struct Event {
    #[allow(dead_code)]
    id: String,
    name: String,
    time: chrono::DateTime<chrono::Utc>,
    link: String,
}

// Syncs Discord with the state of the Redis database
pub fn create_sync_discord_task(
    redis_client: redis::Client,
    discord_api: crate::discord_bot::CacheAndHttp,
    bot_id: u64,
    recurring: bool,
) -> impl FnMut(&mut white_rabbit::Context) -> white_rabbit::DateResult + Send + Sync + 'static {
    move |_ctx| {
        let next_sync_time = match sync_discord(&redis_client, &discord_api, bot_id) {
            Err(err) => {
                eprintln!("Discord syncing task failed: {}", err);
                // Retry in a minute
                white_rabbit::Utc::now() + white_rabbit::Duration::minutes(1)
            }
            _ => {
                // Do another sync in 15 minutes
                white_rabbit::Utc::now() + white_rabbit::Duration::minutes(15)
            }
        };
        if recurring {
            white_rabbit::DateResult::Repeat(next_sync_time)
        } else {
            white_rabbit::DateResult::Done
        }
    }
}

pub fn sync_discord(
    redis_client: &redis::Client,
    discord_api: &crate::discord_bot::CacheAndHttp,
    bot_id: u64,
) -> Result<(), crate::BoxedError> {
    let redis_series_key = "event_series";
    let mut con = redis_client.get_connection()?;
    let event_series: Vec<String> = con.smembers(redis_series_key)?;
    let mut some_failed = false;
    for series in &event_series {
        if let Err(err) = sync_event_series(series, &mut con, discord_api, bot_id) {
            some_failed = true;
            eprintln!("Discord event series syncing task failed: {}", err);
        }
    }
    if some_failed {
        Err(SimpleError::new("One or more discord event series syncs failed").into())
    } else {
        Ok(())
    }
}

/*
For each event series:
  - create a channel if it doesn't exist yet
  - store it in Redis
  - create a player role if it doesn't exist yet
  - store it in Redis
  - create a host role if it doesn't exist yet
  - store it in Redis
  - adjust channel permission overwrites if necessary
  - find all enrolled Meetup users
  - map those Meetup users to Discord users if possible
  - assign the users (including hosts) the player role
  - assign the hosts the host role
*/
fn sync_event_series(
    series_id: &str,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
    bot_id: u64,
) -> Result<(), crate::BoxedError> {
    // Only sync event series that have events in the future
    let redis_series_events_key = format!("event_series:{}:meetup_events", &series_id);
    let event_ids: Vec<String> = redis_connection.smembers(&redis_series_events_key)?;
    let events: Vec<_> = event_ids
        .into_iter()
        .filter_map(|event_id| {
            let redis_event_key = format!("meetup_event:{}", event_id);
            let tuple: redis::RedisResult<(String, String, String)> =
                redis_connection.hget(&redis_event_key, &["time", "name", "link"]);
            match tuple {
                Ok((time, name, link)) => match chrono::DateTime::parse_from_rfc3339(&time) {
                    Ok(time) => Some(Event {
                        id: event_id,
                        name: name,
                        time: time.with_timezone(&chrono::Utc),
                        link: link,
                    }),
                    Err(err) => {
                        eprintln!("Error parsing event time for event {}: {}", time, err);
                        None
                    }
                },
                Err(err) => {
                    eprintln!("Redis error when querying event time: {}", err);
                    None
                }
            }
        })
        .collect();
    // Filter past events
    let now = chrono::Utc::now();
    let mut upcoming: Vec<_> = events
        .into_iter()
        .filter(|event| event.time > now)
        .collect();
    // Sort by date
    upcoming.sort_unstable_by_key(|event| event.time);
    let next_event = match upcoming.first() {
        Some(event) => event,
        None => {
            println!(
                "Event series \"{}\" seems to have no upcoming events associated with it, not syncing to Discord",
                series_id
            );
            return Ok(());
        }
    };
    let event_name = &next_event.name;
    // Step 0: Figure out the title of this event series
    // Parse the series name from the event title
    let series_name = match EVENT_NAME_REGEX.captures(event_name) {
        Some(captures) => captures.name("name").unwrap().as_str(),
        None => {
            return Err(SimpleError::new(format!(
                "Could not extract a series name from the event \"{}\"",
                event_name
            ))
            .into())
        }
    };
    if series_name.len() < 2 || series_name.len() > 80 {
        return Err(SimpleError::new(format!(
            "Channel name \"{}\" is too short or too long",
            series_name
        ))
        .into());
    }
    // Step 1: Sync the channel
    let channel_id = sync_channel(
        series_name,
        series_id,
        bot_id,
        redis_connection,
        discord_api,
    )?;
    // Step 2: Sync the channel's associated role
    let channel_role_id = sync_role(
        series_name,
        /*is_host_role*/ false,
        channel_id,
        redis_connection,
        discord_api,
    )?;
    // Step 3: Sync the channel's associated host role
    let host_role_name = format!("[Host] {}", series_name);
    let channel_host_role_id = sync_role(
        &host_role_name,
        /*is_host_role*/ true,
        channel_id,
        redis_connection,
        discord_api,
    )?;
    // Step 4: Sync the channel permissions
    sync_channel_permissions(
        channel_id,
        channel_role_id,
        channel_host_role_id,
        bot_id,
        discord_api,
    )?;
    // Step 5: Sync RSVP'd users
    sync_user_role_assignments(
        series_id,
        channel_id,
        channel_role_id,
        /*is_host_role*/ false,
        redis_connection,
        discord_api,
    )?;
    sync_user_role_assignments(
        series_id,
        channel_id,
        channel_host_role_id,
        /*is_host_role*/ true,
        redis_connection,
        discord_api,
    )?;
    // Step 6: Make sure that event hosts have the guild's game master role
    sync_game_master_role(series_id, redis_connection, discord_api)?;
    // Step 7: Keep the channel's topic up-to-date
    sync_channel_topic_and_category(
        series_id,
        channel_id,
        &next_event,
        redis_connection,
        discord_api,
    )?;
    Ok(())
}

fn sync_role(
    role_name: &str,
    is_host_role: bool,
    channel_id: ChannelId,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<RoleId, crate::BoxedError> {
    let max_retries = 1;
    let mut current_num_try = 0;
    loop {
        if current_num_try > max_retries {
            return Err(SimpleError::new("Role sync failed, max retries reached").into());
        }
        current_num_try += 1;
        let role = sync_role_impl(
            role_name,
            is_host_role,
            channel_id,
            redis_connection,
            discord_api,
        )?;
        // Make sure that the role ID that was returned actually exists on Discord
        // First, check the cache
        let role_exists = match GUILD_ID.to_guild_cached(&discord_api.cache) {
            Some(guild) => guild.read().roles.contains_key(&role),
            None => false,
        };
        // If it was not in the cache, check Discord
        let role_exists = if role_exists {
            true
        } else {
            let guild_roles = discord_api.http().get_guild_roles(GUILD_ID.0)?;
            guild_roles
                .iter()
                .any(|guild_role| guild_role.id.0 == role.0)
        };
        if !role_exists {
            // This role does not exist on Discord
            // Delete it from Redis and retry
            let redis_discord_roles_key = if is_host_role {
                "discord_host_roles"
            } else {
                "discord_roles"
            };
            let redis_role_channel_key = if is_host_role {
                format!("discord_host_role:{}:discord_channel", role.0)
            } else {
                format!("discord_role:{}:discord_channel", role.0)
            };
            let redis_channel_role_key = if is_host_role {
                format!("discord_channel:{}:discord_host_role", channel_id.0)
            } else {
                format!("discord_channel:{}:discord_role", channel_id.0)
            };
            redis::transaction(redis_connection, &[&redis_channel_role_key], |con, pipe| {
                let current_role: Option<u64> = con.get(&redis_channel_role_key)?;
                if current_role == Some(role.0) {
                    // Remove the broken role from Redis
                    pipe.del(&redis_channel_role_key)
                        .del(&redis_role_channel_key)
                        .srem(redis_discord_roles_key, role.0)
                        .query(con)
                } else {
                    // It seems like the role changed in the meantime
                    // Don't remove it and retry the loop instead
                    pipe.query(con)
                }
            })?;
            continue;
        } else {
            // The role exists on Discord, so everything is good
            return Ok(role);
        }
    }
}

fn sync_role_impl(
    role_name: &str,
    is_host_role: bool,
    channel_id: ChannelId,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<RoleId, crate::BoxedError> {
    let redis_channel_role_key = if is_host_role {
        format!("discord_channel:{}:discord_host_role", channel_id.0)
    } else {
        format!("discord_channel:{}:discord_role", channel_id.0)
    };
    // Check if the role already exists
    {
        let channel_role: Option<u64> = redis_connection.get(&redis_channel_role_key)?;
        if let Some(channel_role) = channel_role {
            // The role already exists
            return Ok(RoleId(channel_role));
        }
    }
    // The role doesn't exist yet -> try to create it
    let temp_channel_role = GUILD_ID.create_role(discord_api.http(), |role_builder| {
        role_builder
            .name(role_name)
            .permissions(Permissions::empty())
    })?;
    println!(
        "Discord event sync: created new temporary channel role {} \"{}\"",
        temp_channel_role.id.0, &temp_channel_role.name
    );
    let redis_discord_roles_key = if is_host_role {
        "discord_host_roles"
    } else {
        "discord_roles"
    };
    let redis_role_channel_key = if is_host_role {
        format!(
            "discord_host_role:{}:discord_channel",
            temp_channel_role.id.0
        )
    } else {
        format!("discord_role:{}:discord_channel", temp_channel_role.id.0)
    };
    let channel_role: redis::RedisResult<(u64,)> =
        redis::transaction(redis_connection, &[&redis_channel_role_key], |con, pipe| {
            let channel_role: Option<u64> = con.get(&redis_channel_role_key)?;
            if channel_role.is_some() {
                // Some role already exists in Redis -> return it
                pipe.get(&redis_channel_role_key).query(con)
            } else {
                // Persist the new role to Redis
                pipe.sadd(redis_discord_roles_key, temp_channel_role.id.0)
                    .ignore()
                    .set(&redis_channel_role_key, temp_channel_role.id.0)
                    .ignore()
                    .set(&redis_role_channel_key, channel_id.0)
                    .ignore()
                    .get(&redis_channel_role_key)
                    .query(con)
            }
        });
    // In case the Redis transaction failed or the role ID returned by Redis
    // doesn't match the newly created role, delete it
    let delete_temp_role = match channel_role {
        Ok((role,)) => role != temp_channel_role.id.0,
        Err(_) => true,
    };
    if delete_temp_role {
        println!("Trying to delete temporary channel role");
        match discord_api
            .http()
            .delete_role(GUILD_ID.0, temp_channel_role.id.0)
        {
            Ok(_) => println!("Successfully deleted temporary channel role"),
            Err(_) => {
                eprintln!(
                    "Could not delete temporary channel role {}",
                    temp_channel_role.id.0
                );
                // Try to persist the information to Redis that we have an orphaned role now
                match redis_connection.sadd("orphaned_discord_roles", temp_channel_role.id.0) {
                    Err(_) => eprintln!(
                        "Could not record orphaned channel role {}",
                        temp_channel_role.id.0
                    ),
                    Ok(()) => println!("Recorded orphaned channel role {}", temp_channel_role.id.0),
                }
            }
        }
    } else {
        println!("Persisted new channel role {}", temp_channel_role.id.0);
    }
    // Return the channel role we got from Redis, no matter
    // if it was newly created or already existing
    channel_role
        .map(|id| RoleId(id.0))
        .map_err(|err| err.into())
}

fn sync_channel(
    channel_name: &str,
    event_series_id: &str,
    bot_id: u64,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<ChannelId, crate::BoxedError> {
    let max_retries = 1;
    let mut current_num_try = 0;
    loop {
        if current_num_try > max_retries {
            return Err(SimpleError::new("Channel sync failed, max retries reached").into());
        }
        current_num_try += 1;
        let channel = sync_channel_impl(
            channel_name,
            event_series_id,
            bot_id,
            redis_connection,
            discord_api,
        )?;
        // Make sure that the channel ID that was returned actually exists on Discord
        let channel_exists = match channel.to_channel(discord_api) {
            Ok(_) => true,
            Err(err) => {
                if let serenity::Error::Http(http_err) = &err {
                    if let serenity::http::HttpError::UnsuccessfulRequest(response) =
                        http_err.as_ref()
                    {
                        if response.status_code == reqwest::StatusCode::NOT_FOUND {
                            false
                        } else {
                            return Err(err.into());
                        }
                    } else {
                        return Err(err.into());
                    }
                } else {
                    return Err(err.into());
                }
            }
        };
        if !channel_exists {
            // This channel does not exist on Discord
            // Delete it from Redis and retry
            let redis_discord_channels_key = "discord_channels";
            let redis_channel_series_key = format!("discord_channel:{}:event_series", channel.0);
            let redis_series_channel_key =
                format!("event_series:{}:discord_channel", event_series_id);
            redis::transaction(
                redis_connection,
                &[&redis_series_channel_key],
                |con, pipe| {
                    let current_channel: Option<u64> = con.get(&redis_series_channel_key)?;
                    if current_channel == Some(channel.0) {
                        // Remove the broken channel from Redis
                        pipe.del(&redis_series_channel_key)
                            .del(&redis_channel_series_key)
                            .srem(redis_discord_channels_key, channel.0)
                            .query(con)
                    } else {
                        // It seems like the channel changed in the meantime
                        // Don't remove it and retry the loop instead
                        pipe.query(con)
                    }
                },
            )?;
            continue;
        } else {
            // The channel exists on Discord, so everything is good
            return Ok(channel);
        }
    }
}

fn sync_channel_impl(
    channel_name: &str,
    event_series_id: &str,
    bot_id: u64,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<ChannelId, crate::BoxedError> {
    let redis_series_channel_key = format!("event_series:{}:discord_channel", event_series_id);
    // Check if the channel already exists
    {
        let channel: Option<u64> = redis_connection.get(&redis_series_channel_key)?;
        if let Some(channel) = channel {
            // The channel already exists
            return Ok(ChannelId(channel));
        }
    }
    // The channel doesn't exist yet -> try to create it
    // The @everyone role has the same id as the guild
    let role_everyone_id = RoleId(GUILD_ID.0);
    let permission_overwrites = vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::READ_MESSAGES,
            kind: PermissionOverwriteType::Role(role_everyone_id),
        },
        PermissionOverwrite {
            allow: Permissions::READ_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(UserId(bot_id)),
        },
    ];
    let temp_channel = GUILD_ID.create_channel(discord_api.http(), |channel_builder| {
        channel_builder
            .name(channel_name)
            .permissions(permission_overwrites)
    })?;
    println!(
        "Discord event sync: created new temporary channel {} \"{}\"",
        temp_channel.id.0, &temp_channel.name
    );
    let redis_discord_channels_key = "discord_channels";
    let redis_channel_series_key = format!("discord_channel:{}:event_series", temp_channel.id.0);
    let channel: redis::RedisResult<(u64,)> = redis::transaction(
        redis_connection,
        &[&redis_series_channel_key],
        |con, pipe| {
            let channel: Option<u64> = con.get(&redis_series_channel_key)?;
            if channel.is_some() {
                // Some channel already exists in Redis -> return it
                pipe.get(&redis_series_channel_key).query(con)
            } else {
                // Persist the new channel to Redis
                pipe.sadd(redis_discord_channels_key, temp_channel.id.0)
                    .ignore()
                    .set(&redis_series_channel_key, temp_channel.id.0)
                    .ignore()
                    .set(&redis_channel_series_key, event_series_id)
                    .ignore()
                    .get(&redis_series_channel_key)
                    .query(con)
            }
        },
    );
    // In case the Redis transaction failed or the channel ID returned by Redis
    // doesn't match the newly created channel, delete it
    let delete_temp_channel = match channel {
        Ok((channel,)) => channel != temp_channel.id.0,
        Err(_) => true,
    };
    if delete_temp_channel {
        println!("Trying to delete temporary channel");
        match discord_api.http().delete_channel(temp_channel.id.0) {
            Ok(_) => println!("Successfully deleted temporary channel"),
            Err(_) => {
                eprintln!("Could not delete temporary channel {}", temp_channel.id.0);
                // Try to persist the information to Redis that we have an orphaned channel now
                match redis_connection.sadd("orphaned_discord_channels", temp_channel.id.0) {
                    Err(_) => eprintln!("Could not record orphaned channel {}", temp_channel.id.0),
                    Ok(()) => println!("Recorded orphaned channel {}", temp_channel.id.0),
                }
            }
        }
    } else {
        println!("Persisted new channel {}", temp_channel.id.0);
    }
    // Return the channel we got from Redis, no matter
    // if it was newly created or already existing
    channel.map(|id| ChannelId(id.0)).map_err(|err| err.into())
}

// Makes sure that the Discord channel has the appropriate permission
// overwrites for the channel's role and host role.
// Specifically does not remove any additional permission overwrites
// that the channel might have.
fn sync_channel_permissions(
    channel_id: ChannelId,
    role_id: RoleId,
    host_role_id: RoleId,
    bot_id: u64,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<(), crate::BoxedError> {
    // The @everyone role has the same id as the guild
    let role_everyone_id = RoleId(GUILD_ID.0);
    // Make this channel private.
    // This is achieved by denying @everyone the READ_MESSAGES permission
    // but allowing the now role the READ_MESSAGES permission.
    // see: https://support.discordapp.com/hc/en-us/articles/206143877-How-do-I-set-up-a-Role-Exclusive-channel-
    let permission_overwrites = [
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::READ_MESSAGES,
            kind: PermissionOverwriteType::Role(role_everyone_id),
        },
        PermissionOverwrite {
            allow: Permissions::READ_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Member(UserId(bot_id)),
        },
        PermissionOverwrite {
            allow: Permissions::READ_MESSAGES | Permissions::MENTION_EVERYONE,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(role_id),
        },
        PermissionOverwrite {
            allow: Permissions::READ_MESSAGES
                | Permissions::MENTION_EVERYONE
                | Permissions::MANAGE_MESSAGES,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(host_role_id),
        },
    ];
    for permission_overwrite in &permission_overwrites {
        channel_id.create_permission(discord_api.http(), permission_overwrite)?;
    }
    Ok(())
}

fn sync_user_role_assignments(
    event_series_id: &str,
    channel: ChannelId,
    role: RoleId,
    is_host_role: bool,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<(), crate::BoxedError> {
    // First, find all events belonging to this event series
    let redis_series_events_key = format!("event_series:{}:meetup_events", &event_series_id);
    let event_ids: Vec<String> = redis_connection.smembers(&redis_series_events_key)?;
    if event_ids.is_empty() {
        println!(
            "Event series \"{}\" seems to have no events associated with it, not syncing to Discord",
            event_series_id
        );
        return Ok(());
    }
    // Then, find all Meetup users RSVP'd to those events
    let redis_event_users_keys: Vec<_> = event_ids
        .iter()
        .map(|event_id| {
            if is_host_role {
                format!("meetup_event:{}:meetup_hosts", event_id)
            } else {
                format!("meetup_event:{}:meetup_users", event_id)
            }
        })
        .collect();
    let (meetup_user_ids,): (Vec<u64>,) = redis::pipe()
        .sunion(redis_event_users_keys)
        .query(redis_connection)?;
    // Now, try to associate the RSVP'd Meetup users with Discord users
    let discord_user_ids: Result<Vec<Option<u64>>, _> = meetup_user_ids
        .into_iter()
        .map(|meetup_id| {
            let redis_meetup_discord_key = format!("meetup_user:{}:discord_user", meetup_id);
            redis_connection.get(&redis_meetup_discord_key)
        })
        .collect();
    // Filter the None values
    let discord_user_ids: Vec<_> = discord_user_ids?.into_iter().filter_map(|id| id).collect();
    // Check whether any users have manually removed roles and don't add them back
    let redis_channel_removed_hosts_key = format!("discord_channel:{}:removed_hosts", channel.0);
    let redis_channel_removed_users_key = format!("discord_channel:{}:removed_users", channel.0);
    let ignore_discord_user_ids: Vec<u64> = if is_host_role {
        // Don't automatically assign the host role to users that have either
        // been manually removed as a host or as a user from a channel
        redis_connection.sunion(&[
            &redis_channel_removed_hosts_key,
            &redis_channel_removed_users_key,
        ])?
    } else {
        // Don't automatically assign the user role to user that have been
        // manually removed from a channel
        redis_connection.smembers(&redis_channel_removed_users_key)?
    };
    // Lastly, actually assign the role to the Discord users
    for user_id in discord_user_ids {
        if ignore_discord_user_ids.contains(&user_id) {
            continue;
        }
        match UserId(user_id).to_user(discord_api) {
            Ok(user) => match user.has_role(discord_api, GUILD_ID, role) {
                Ok(has_role) => {
                    if !has_role {
                        match discord_api
                            .http()
                            .add_member_role(GUILD_ID.0, user_id, role.0)
                        {
                            Ok(_) => println!("Assigned user {} to role {}", user_id, role.0),
                            Err(err) => eprintln!(
                                "Could not assign user {} to role {}: {}",
                                user_id, role.0, err
                            ),
                        }
                    }
                }
                Err(err) => eprintln!(
                    "Could not figure out whether the user {} already has role {}: {}",
                    user.id, role.0, err
                ),
            },
            Err(err) => eprintln!("Could not find the user {}: {}", user_id, err),
        }
    }
    Ok(())
}

fn sync_game_master_role(
    event_series_id: &str,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<(), crate::BoxedError> {
    if let Some(game_master_role) = GAME_MASTER_ID {
        // First, find all events belonging to this event series
        let redis_series_events_key = format!("event_series:{}:meetup_events", &event_series_id);
        let event_ids: Vec<String> = redis_connection.smembers(&redis_series_events_key)?;
        if event_ids.is_empty() {
            return Ok(());
        }
        // Then, find all Meetup host of those events
        let redis_event_hosts_keys: Vec<_> = event_ids
            .iter()
            .map(|event_id| format!("meetup_event:{}:meetup_hosts", event_id))
            .collect();
        let (meetup_host_ids,): (Vec<u64>,) = redis::pipe()
            .sunion(redis_event_hosts_keys)
            .query(redis_connection)?;
        // Now, try to associate the hosts with Discord users
        let redis_meetup_host_discord_keys: Vec<_> = meetup_host_ids
            .into_iter()
            .map(|meetup_id| format!("meetup_user:{}:discord_user", meetup_id))
            .collect();
        let discord_host_ids: Vec<Option<u64>> = redis::cmd("MGET")
            .arg(redis_meetup_host_discord_keys)
            .query(redis_connection)?;
        // Filter the None values
        let discord_host_ids: Vec<_> = discord_host_ids.into_iter().filter_map(|id| id).collect();
        // Lastly, actually assign the Game Master role to the hosts
        for host_id in discord_host_ids {
            match UserId(host_id).to_user(discord_api) {
                Ok(user) => match user.has_role(discord_api, GUILD_ID, game_master_role) {
                    Ok(has_role) => {
                        if !has_role {
                            match discord_api.http().add_member_role(GUILD_ID.0, host_id, game_master_role.0) {
                                Ok(_) => println!("Assigned user {} to the game master role", host_id),
                                Err(err) => eprintln!("Could not assign user {} to the game master role: {}", host_id, err),
                            }
                        }
                    }
                    Err(err) => eprintln!(
                        "Could not figure out whether the user {} already has the game master role: {}",
                        user.id, err
                    ),
                },
                Err(err) => eprintln!("Could not find the host user {}: {}", host_id, err),
            }
        }
    }
    Ok(())
}

fn sync_channel_topic_and_category(
    series_id: &str,
    channel_id: ChannelId,
    next_event: &Event,
    redis_connection: &mut redis::Connection,
    discord_api: &crate::discord_bot::CacheAndHttp,
) -> Result<(), crate::BoxedError> {
    // Sync the topic and the category
    let topic = format!("Next session: {}", &next_event.link);
    let redis_series_type_key = format!("event_series:{}:type", series_id);
    let event_type: Option<String> = redis_connection.get(&redis_series_type_key)?;
    let category = match event_type.as_ref().map(String::as_str) {
        Some("campaign") => CAMPAIGN_CATEGORY_ID,
        Some("adventure") => ONE_SHOT_CATEGORY_ID,
        _ => {
            eprintln!(
                "Event series {} does not have a type of 'campaign' or 'adventure'",
                series_id
            );
            CAMPAIGN_CATEGORY_ID
        }
    };
    let channel = channel_id.to_channel(discord_api)?;
    if let serenity::model::channel::Channel::Guild(channel) = channel {
        let channel_needs_update = {
            let channel_lock = channel.read();
            let current_topic = &channel_lock.topic;
            let topic_needs_update = if let Some(current_topic) = current_topic {
                current_topic != &topic
            } else {
                true
            };
            let category_needs_update = if category.is_some() {
                category != channel_lock.category_id
            } else {
                false
            };
            topic_needs_update || category_needs_update
        };
        if channel_needs_update {
            channel_id.edit(&discord_api.http, |channel_edit| {
                channel_edit.topic(topic);
                if category.is_some() {
                    channel_edit.category(category);
                }
                channel_edit
            })?;
        }
    }
    Ok(())
}
